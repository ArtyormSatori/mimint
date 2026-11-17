package com.mimint.shell

import android.content.Context
import java.io.DataInputStream
import java.io.DataOutputStream
import java.io.EOFException
import java.io.IOException
import java.net.InetAddress
import java.net.ServerSocket
import java.net.Socket
import java.nio.charset.StandardCharsets
import java.security.KeyStore
import java.security.MessageDigest
import javax.net.ssl.KeyManagerFactory
import javax.net.ssl.SSLContext
import javax.net.ssl.SSLServerSocket
import org.json.JSONObject
import kotlin.concurrent.thread

/**
 * Protocol v2 server (see `crates/mimint-body/src/android.rs`
 * for the authoritative wire spec — this file implements it,
 * it does not redefine it).
 *
 * Framing: u32 big-endian JSON length + JSON header + payload
 * bytes. Requests carry `v`, `id`, `method`, `params`, `auth`,
 * `role`, `payload_len`. Responses echo `v`/`id` and carry
 * `ok`, `result`/`error`, `payload_len`.
 *
 * Rules enforced here (never trusted to the client):
 * - `v` must be 2.
 * - `auth` must equal the pairing token, compared with
 *   [MessageDigest.isEqual] (no early exit).
 * - Guests may call only display_size, screenshot,
 *   permissions, focused_window.
 * - JSON headers are capped at 1 MiB, payloads at 64 MiB.
 * - The socket binds 127.0.0.1 only. Reaching it from a
 *   computer requires `adb reverse` — a deliberate,
 *   physical-access act by the device owner.
 *
 * TLS: when [tlsEnabled], the server socket is an
 * SSLServerSocket using the keystore identity; the client
 * pins the certificate SHA-256. Plain mode exists only for
 * debugging on loopback and must be enabled explicitly in
 * the UI (default: TLS on).
 *
 * `kill` stops the server and the service. The server thread
 * ends; the service's onDestroy closes the rest.
 */
class ProtocolServer(
    private val context: Context,
    private val port: Int,
    private val tlsEnabled: Boolean,
    private val capture: ScreenCapture,
    private val injector: InputInjector,
    private val focusedWindow: () -> FocusedWindow?,
    private val onKill: () -> Unit,
) {
    data class FocusedWindow(val id: Long, val clazz: String, val title: String)

    private val audit = AuditLog(context)
    private val oracle = OracleLog(context)
    private var serverSocket: ServerSocket? = null
    private var acceptThread: Thread? = null
    @Volatile private var running = false

    /** The target geometry the oracle log uses for win_x/win_y. */
    @Volatile var targetGeom: OracleLog.Geom = OracleLog.Geom(0, 0, 0, 0)

    fun start() {
        val socket = if (tlsEnabled) tlsServerSocket() else ServerSocket()
        socket.reuseAddress = true
        socket.bind(java.net.InetSocketAddress(InetAddress.getByName("127.0.0.1"), port))
        serverSocket = socket
        running = true
        acceptThread = thread(name = "mimint-accept", isDaemon = true) {
            while (running) {
                try {
                    val client = socket.accept()
                    thread(name = "mimint-conn", isDaemon = true) {
                        handleConnection(client)
                    }
                } catch (_: IOException) {
                    // Socket closed by stop(): the loop ends.
                    if (running) Thread.sleep(50)
                }
            }
        }
    }

    fun stop() {
        running = false
        try { serverSocket?.close() } catch (_: IOException) {}
        serverSocket = null
    }

    private fun tlsServerSocket(): SSLServerSocket {
        val ks = KeyStore.getInstance("AndroidKeyStore").also { it.load(null) }
        // Touch the identity so it exists before the factory reads it.
        IdentityStore.certificateChain()
        val kmf = KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm())
        kmf.init(ks, null)
        val sslContext = SSLContext.getInstance("TLS")
        sslContext.init(kmf.keyManagers, null, null)
        val factory = sslContext.serverSocketFactory
        val socket = factory.createServerSocket() as SSLServerSocket
        socket.enabledProtocols = arrayOf("TLSv1.3", "TLSv1.2")
        return socket
    }

    private fun handleConnection(socket: Socket) {
        socket.soTimeout = 30_000
        try {
            val input = DataInputStream(socket.getInputStream())
            val output = DataOutputStream(socket.getOutputStream())
            while (running) {
                val headerLen = try {
                    input.readInt()
                } catch (_: EOFException) {
                    break
                }
                if (headerLen < 0 || headerLen > 1024 * 1024) break
                val headerBytes = ByteArray(headerLen)
                input.readFully(headerBytes)
                val request = try {
                    JSONObject(String(headerBytes, StandardCharsets.UTF_8))
                } catch (_: Exception) {
                    break
                }
                val payloadLen = request.optInt("payload_len", 0)
                if (payloadLen < 0 || payloadLen > 64 * 1024 * 1024) break
                if (payloadLen > 0) {
                    // Requests carry no payload today; drain
                    // anything sent so framing stays aligned.
                    input.skipBytes(payloadLen)
                }
                val (reply, payload) = dispatch(request)
                val replyBytes = reply.toString().toByteArray(StandardCharsets.UTF_8)
                output.writeInt(replyBytes.size)
                output.write(replyBytes)
                output.write(payload)
                output.flush()
                if (request.optString("method") == "kill") {
                    // The reply is flushed; now die.
                    thread(isDaemon = true) {
                        Thread.sleep(100)
                        onKill()
                    }
                    break
                }
            }
        } catch (_: IOException) {
            // Client went away; nothing to report.
        } finally {
            try { socket.close() } catch (_: IOException) {}
        }
    }

    private fun dispatch(request: JSONObject): Pair<JSONObject, ByteArray> {
        val id = request.optLong("id", 0)
        val method = request.optString("method", "")
        if (request.optInt("v", -1) != 2) {
            audit.record(method, "version_mismatch")
            return errorReply(id, "version_mismatch", "protocol version must be 2") to ByteArray(0)
        }
        val token = IdentityStore.pairingToken(context)
        val auth = request.optString("auth", "")
        if (!MessageDigest.isEqual(
                auth.toByteArray(StandardCharsets.UTF_8),
                token.toByteArray(StandardCharsets.UTF_8),
            )
        ) {
            audit.record("auth", "unauthorized")
            return errorReply(id, "unauthorized", "pairing token rejected") to ByteArray(0)
        }
        val role = request.optString("role", "")
        if (role != "owner" && role != "guest") {
            return errorReply(id, "invalid_argument", "role must be owner or guest") to ByteArray(0)
        }
        if (role == "guest" && method !in setOf(
                "display_size", "screenshot", "permissions", "focused_window",
            )
        ) {
            audit.record(method, "forbidden_role")
            return errorReply(id, "forbidden_role", "guest role may only read the screen") to ByteArray(0)
        }
        val params = request.optJSONObject("params") ?: JSONObject()
        val (reply, outcome) = handleMethod(id, method, params)
        audit.record(method, outcome)
        return reply
    }

    /**
     * Runs one method. Returns the (reply header, payload) pair
     * plus the audit outcome string.
     */
    private fun handleMethod(
        id: Long,
        method: String,
        params: JSONObject,
    ): Pair<Pair<JSONObject, ByteArray>, String> {
        fun error(code: String, message: String) =
            Pair(errorReply(id, code, message) to ByteArray(0), code)
        fun ok(result: JSONObject, payload: ByteArray = ByteArray(0)) =
            Pair(okReply(id, result, payload) to payload, "ok")
        fun okNull() =
            Pair(okReplyNull(id) to ByteArray(0), "ok")

        return when (method) {
            "display_size" -> {
                val (w, h) = capture.size
                ok(JSONObject().put("width", w).put("height", h))
            }
            "permissions" -> ok(
                JSONObject()
                    .put("screen_capture", if (capture.granted) "granted" else "denied")
                    .put("input_control", "granted"),
            )
            "focused_window" -> {
                val focused = focusedWindow()
                if (focused == null) {
                    // A null result: the client reads None.
                    okNull()
                } else {
                    ok(
                        JSONObject()
                            .put("window_id", focused.id)
                            .put("class", focused.clazz)
                            .put("title", focused.title),
                    )
                }
            }
            "screenshot" -> {
                val frame = capture.latestFrame()
                    ?: return error(
                        "permission_denied",
                        "screen capture is not granted; approve it in the Mimint Shell app",
                    )
                val (w, h) = capture.size
                ok(
                    JSONObject()
                        .put("width", w)
                        .put("height", h)
                        .put("scale_factor", capture.density.toDouble())
                        .put(
                            "origin",
                            org.json.JSONArray().put(0).put(0),
                        ),
                    frame,
                )
            }
            "kill" -> ok(JSONObject())
            "click" -> {
                val (x, y) = params.ints("x", "y") ?: return error(
                    "invalid_argument",
                    "click needs integer x and y",
                )
                if (!inBounds(x, y)) return error("out_of_bounds", "($x, $y) outside display")
                when (val r = injector.click(x, y)) {
                    is InputInjector.Outcome.Ok -> {
                        oracle.click(x, y, targetGeom)
                        ok(JSONObject())
                    }
                    is InputInjector.Outcome.Error -> error(r.code, r.message)
                }
            }
            "move" -> {
                val (x, y) = params.ints("x", "y") ?: return error(
                    "invalid_argument",
                    "move needs integer x and y",
                )
                if (!inBounds(x, y)) return error("out_of_bounds", "($x, $y) outside display")
                // No pointer exists on touch-only Android: the
                // move is a documented no-op, not a gesture.
                ok(JSONObject())
            }
            "type_text" -> {
                val text = params.optString("text", "")
                when (val r = injector.typeText(text)) {
                    is InputInjector.Outcome.Ok -> {
                        oracle.typed(text)
                        ok(JSONObject())
                    }
                    is InputInjector.Outcome.Error -> error(r.code, r.message)
                }
            }
            "scroll" -> {
                val (x, y) = params.ints("x", "y") ?: return error(
                    "invalid_argument",
                    "scroll needs integer x and y",
                )
                val dx = params.optInt("dx", 0)
                val dy = params.optInt("dy", 0)
                if (!inBounds(x, y)) return error("out_of_bounds", "($x, $y) outside display")
                when (val r = injector.scroll(x, y, dx, dy)) {
                    is InputInjector.Outcome.Ok -> {
                        oracle.scroll(x, y, dx, dy, targetGeom)
                        ok(JSONObject())
                    }
                    is InputInjector.Outcome.Error -> error(r.code, r.message)
                }
            }
            "drag" -> {
                val from = params.ints("from_x", "from_y")
                val to = params.ints("to_x", "to_y")
                if (from == null || to == null) return error(
                    "invalid_argument",
                    "drag needs integer from_x, from_y, to_x, to_y",
                )
                if (!inBounds(from.first, from.second) || !inBounds(to.first, to.second)) {
                    return error("out_of_bounds", "drag endpoint outside display")
                }
                when (val r = injector.drag(from.first, from.second, to.first, to.second)) {
                    is InputInjector.Outcome.Ok -> {
                        oracle.drag(from.first, from.second, to.first, to.second, targetGeom)
                        ok(JSONObject())
                    }
                    is InputInjector.Outcome.Error -> error(r.code, r.message)
                }
            }
            "double_click" -> {
                val (x, y) = params.ints("x", "y") ?: return error(
                    "invalid_argument",
                    "double_click needs integer x and y",
                )
                if (!inBounds(x, y)) return error("out_of_bounds", "($x, $y) outside display")
                when (val r = injector.doubleClick(x, y)) {
                    is InputInjector.Outcome.Ok -> {
                        oracle.doubleClick(x, y, targetGeom)
                        ok(JSONObject())
                    }
                    is InputInjector.Outcome.Error -> error(r.code, r.message)
                }
            }
            "right_click" -> error(
                "unsupported",
                "right-click is not available on touch-only Android",
            )
            "key", "hold_key" -> error(
                "unsupported",
                "an AccessibilityService cannot inject key events",
            )
            else -> error("unknown_method", "unknown method $method")
        }
    }

    // One reply shape, everywhere: the request id is echoed,
    // `v` is always 2, and payload_len matches the bytes that
    // follow the header.
    private fun errorReply(id: Long, code: String, message: String): JSONObject =
        JSONObject()
            .put("v", 2).put("id", id).put("ok", false)
            .put("error", JSONObject().put("code", code).put("message", message))
            .put("payload_len", 0)

    private fun okReply(id: Long, result: JSONObject, payload: ByteArray): JSONObject =
        JSONObject()
            .put("v", 2).put("id", id).put("ok", true)
            .put("result", result)
            .put("payload_len", payload.size)

    private fun okReplyNull(id: Long): JSONObject =
        JSONObject()
            .put("v", 2).put("id", id).put("ok", true)
            .put("result", JSONObject.NULL)
            .put("payload_len", 0)

    private fun inBounds(x: Int, y: Int): Boolean {
        val (w, h) = capture.size
        return x in 0 until w && y in 0 until h
    }

    private fun JSONObject.ints(a: String, b: String): Pair<Int, Int>? {
        if (!has(a) || !has(b)) return null
        return try {
            getInt(a) to getInt(b)
        } catch (_: Exception) {
            null
        }
    }
}

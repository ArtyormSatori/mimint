package com.mimint.shell

import android.content.Context
import java.io.File
import org.json.JSONObject

/**
 * The shell's audit trail: one JSON object per line, method +
 * outcome only. Never pixels, never typed text, never the
 * pairing token. Same shape as the fake server's audit lines,
 * so the conformance hook inspects both the same way.
 */
class AuditLog(context: Context) {
    private val file = File(context.filesDir, "audit.jsonl")

    @Synchronized
    fun record(method: String, outcome: String) {
        val line = JSONObject()
            .put("method", method)
            .put("outcome", outcome)
            .toString()
        file.appendText(line + "\n")
    }
}

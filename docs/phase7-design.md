# Thiết kế Phase 7 — Friends/sync 4 body + đa kênh desktop + robot ESP32 (BẢN CUỐI ĐÃ REVIEW)

- **Trạng thái:** BẢN CUỐI ĐÃ REVIEW + SIGN-OFF — vòng review thiết kế đã xong ngày 19/11/2026 (xem bảng sign-off cuối §14); mọi điểm J1–J11 đều đã có quyết định cuối D-DESIGN7-1..11. Bản này là căn cứ duy nhất cho build Phase 7.
- **Ngày chốt:** 2026-11-19. **Người viết:** vòng soạn thiết kế (giai đoạn 1, không code); vòng viết cuối áp danh sách sửa F7D-01..F7D-25 từ `REVIEW-SYNTHESIS-DESIGN.md`.
- **HEAD xuất phát:** `a431c428ac46456b553a6928878f22e104ce424b` (đã xác nhận bằng `git rev-parse` trên clone chỉ-đọc `/home/adminis/mimint-p7-design-read` trên vega, clone mới từ remote `main`; không commit, không sửa cây). Lưu ý: cây `/home/adminis/mimint` trên vega đang ở `f178318` (Phase 3, cũ) — bản thiết kế này đọc code tại `a431c42`, không phải cây cũ đó.
- **Nguồn bắt buộc:** `CRITERIA.md` Phase 7 (P7.0 10 mục + P7.1–P7.6 + cổng G1–G4), `SYNTHESIS-PLAN.md` §5–§9 (D1/D6/D7/D11/D13), `PROPOSALS-SYNTHESIS.md` (P0-1/P0-2/P0-13/P0-18, P1 selective sync, §5 KHÔNG làm, §6 BodyWsServer), `phase0/lane2-tls-wss.md` (U1/U2 FAIL + điều kiện an toàn đường XiaoZhi), `GAP-CHECK-2-REPO.md` + `gap-lane2-xiaozhi.md`, `REVIEW-SYNTHESIS-DESIGN.md` (quyết định D-DESIGN7-1..11, xử lý H1–H7, danh sách sửa F7D-01..F7D-25).
- **Quy tắc D1 (clean-room):** protocol v2 friends/sync trong bản này viết mới hoàn toàn bằng lời nhà. Không copy code, byte-format, frame, tên trường, hay chuỗi salt/label nào của andee gốc. Cơ chế P-256 ECDH + AES-256-GCM + mã 6 số chỉ được diễn đạt lại ở mức ý tưởng (xem §0.6). Mọi điểm tái dùng đều là tái dùng code của chính cây Mimint, nêu file:line tại HEAD; phần nào mới ghi rõ MỚI.
- **Quy tắc không nhập:** không tài khoản cloud, không relay, không sync vault/secret/history, không học ké, không marketplace skill, không fine-tune, không Best-of-N, không grounder D12, không zbus, không ký số, không huấn luyện thật, không engine voice/proactive/scheduler, không mDNS (chỉ thiết kế tương lai ở §13.2). Mọi khẳng định về crypto trong bản này đều dựa trên probe chạy thật ở §0.3, không suy đoán.

## 0. Phạm vi, nguyên tắc, bằng chứng

### 0.1 Phạm vi

Bản thiết kế quyết: threat model; protocol v2 friends/sync (envelope, handshake, versioning, conflict); pairing ceremony (SAS 6 số, fingerprint, 2 mức trust, thu hồi, xoay key); tập sync + luật không merge âm thầm + chính sách xóa; hub server xác thực + mã hóa; phê duyệt xuyên thiết bị; handoff có lease; body ESP32 hai đường trong ràng buộc Phase 0 Lane 2; quyết định Privacy Mode; ngân sách hiệu năng máy yếu. Mọi mục ở §13 (BỊ CHẶN / còn mở) bản này KHÔNG quyết thay.

"Đa kênh desktop" trong tiêu đề phase được hiểu đúng như CRITERIA ghi: P0-13 (ma trận tin cậy kênh + phê duyệt xuyên thiết bị) + P0-18 (auth + mã hóa MỌI kênh điều khiển, gồm thiết kế lại BodyWsServer). Bản này không mở rộng phạm vi sang bot chat (Telegram/WhatsApp/Discord) hay kênh từ xa nào khác.

Từ vựng dùng trong bản này (thống nhất, tránh nhầm với từ của andee):

| Từ | Nghĩa trong bản này |
|---|---|
| Body | Một thiết bị chạy Mimint: desktop (Linux/macOS/Windows), Android, ESP32. Mỗi body có SQLite riêng (D6). |
| Device / thiết bị | Đồng nghĩa body, dùng khi nói về danh tính + registry. |
| Peer | Một body khác đã pair với body này. Peer lạ = chưa pair. |
| Hub | Máy desktop chạy não và phục vụ các body khác (P0-18). Mọi body desktop đều có thể là hub; không có hub trung tâm. |
| Owner / Guest | Hai mức trust của một pairing (xem §3.5). Owner là chính user; guest là người/thiết bị được chia sẻ có giới hạn. |
| Session | Một kết nối đã bắt tay xong giữa hai body, có khóa phiên riêng. |
| Record | Một đơn vị dữ liệu sync (một note, một SOUL file, một skill, một profile, một todo/plan). |

### 0.2 Nguyên tắc (kế thừa cây hiện tại, không phát minh lại)

1. **Deny-by-default, fail-closed.** Mọi quyết định mơ hồ đều nghiêng về từ chối + ghi audit. Audit ghi không được thì việc không chạy (tiền lệ `AuditLog`, `guards.rs`). Peer lạ bị từ chối TRƯỚC khi trao đổi dữ liệu.
2. **Một body, một SQLite, một não.** Sync không tạo não chung, không tạo database chung. Mỗi body chạy độc lập được khi mất peer (D6); sync chỉ sao chép record đã chọn, không bao giờ là đường sống còn của lượt chạy.
3. **Pin là danh tính, không PKI.** Như Phase 4: SHA-256 của khóa/certificate đã pin thay toàn bộ PKI — không CA, không hostname check, không trust store. Khóa pin chỉ đến từ pairing ceremony (mắt + bàn phím), không bao giờ từ mạng.
4. **Không thay đổi hành vi khi tính năng tắt.** Khi không có peer nào được pair và hub không bật LAN, hành vi single-body phải y hệt hôm nay: suite cũ chạy không đổi, không listener mới, không file mới bắt buộc. Ngoại lệ duy nhất được phép: migration thêm cột `sync_id` cho notebook (§4.3) và thêm trường vào `detail` của audit — cả hai đều tương thích ngược, đọc dữ liệu cũ nguyên vẹn.
5. **Bằng chứng bằng máy.** Mọi cơ chế mới đều có pass test nêu tên ở §11. Không test thì không coi là có. Test Phase 7 chạy trên 2 instance thật trên vega, không chỉ unit test với key giả (CRITERIA P7.1/P7.2/P7.6).
6. **0 dependency mới — ưu tiên tuyệt đối.** Chỉ `ring` 0.17.14 + `rustls` 0.23.46 + các crate đã có trong cây (`serde_json`, `rusqlite`, `tokio`, `zeroize`). Mọi chỗ thiết kế cần thứ chưa có trong cây đều ghi rõ là MỚI và tự triển khai bằng std + crate đã có (framing WebSocket tối thiểu §8.2, sinh id ngẫu nhiên §4.3, hex §3.2); không xin dep mới nào trong bản này. Nếu vòng review code ở build thấy cần dep mới thì phải chứng minh ring/rustls không làm được + qua cổng license G3, và leo thang về parent trước khi thêm (xem §8.2 về điều kiện leo thang của riêng WebSocket).

### 0.3 Bằng chứng kiểm chứng trên vega (bước bắt buộc)

Máy: Debian 13, kernel `6.12.113+deb13-amd64`, user `adminis`, không sudo, CPU Intel Core i5-2520M, 16 GB RAM. Toolchain: rustc/cargo 1.98.1. Toàn bộ log nằm ở `design-evidence/` (bản local):

| Log | Nội dung | Kết quả |
|---|---|---|
| `ring-probe-run.log` (+ source `ring-probe/`) | Scratch crate pin `ring = "=0.17.14"`, chạy `cargo run --release --offline -j 2` trên vega | **PASS, exit 0.** 2 keypair P-256 (pubkey 65 byte, khác nhau); ECDH 2 chiều + HKDF-SHA256 ra cùng khóa phiên 32 byte; AES-256-GCM mã hóa + giải mã 1 message (25 → 41 byte, tag 16 byte); khóa sai PHẢI fail (fail thật); ciphertext sửa 1 bit PHẢI fail; AAD sai PHẢI fail. Bench: keygen 24 µs/op; cặp ECDH+HKDF 445 µs/cặp; seal 1 KiB 1 µs/op |
| `api-audit.log` (+ script `probe-bc-api-audit.sh`) | Grep chỉ-đọc cây tại `a431c42` + source vendored ring/rustls | Pinning Phase 4: phía client là production, phía server TLS chỉ có trong fake server test; **không có code sinh chứng chỉ** (rcgen 0 hit), **không có WebSocket dep** (tungstenite 0 hit); rustls có đủ exporter/verifier/`StreamOwned`/builder; ring có đủ ECDH/AEAD/HKDF/ECDSA/Ed25519/`SystemRandom` |
| `README.md` (trong `design-evidence/`) | Cái gì đã chứng minh / cái gì chưa + cách chạy lại | Chưa chứng minh: handshake thật đầu-cuối, đo 2 instance, phần cứng ESP32/Mac/Windows/Android, sinh cert X.509, WebSocket server |

Kết luận duy nhất được phép rút ra: **ring 0.17.14 làm được toàn bộ mảnh crypto mà thiết kế này cần, đã chạy thật trên vega.** Mọi cơ chế nối các mảnh đó thành protocol (handshake, envelope, sync, hub) đều CHƯA tồn tại và phải viết mới ở giai đoạn 2 theo bản này.

### 0.4 Điểm chạm code hiện có (đọc thật tại HEAD, tóm tắt)

Mọi số dòng dưới đây là tại HEAD `a431c42`, file tương đối từ gốc repo. "TÁI DÙNG" = dùng lại như-is hoặc mở rộng hẹp; "MỚI" = chưa tồn tại.

| Module | Sự thật đã đọc (làm nền cho thiết kế) | Phase 7 dùng như thế nào |
|---|---|---|
| `crates/mimint-body/src/android.rs:1-129` | Protocol v2 Android: TCP loopback, framing u32 BE + JSON header + payload; `v` phải là 2; header ≤ 1 MiB, payload ≤ 64 MiB; `id` request/response phải khớp. | TÁI DÙNG hình thức framing (u32 BE + JSON + payload) cho friends protocol §2 — đây là hình thức của chính cây Mimint, không phải của andee. Nội dung envelope friends là MỚI, khác hẳn method/params của body protocol. |
| `android.rs:155-178` (`Role`), `:1461-1469` | `Role::{Owner, Guest}`; guest chỉ được 4 method đọc màn hình, server-side enforce, vi phạm → `forbidden_role`. | TÁI DÙNG mô hình 2 mức trust + enforce phía server cho pairing §3.5 và hub §5.3. Tập method guest của hub là MỚI, hẹp hơn nữa. |
| `android.rs:196-226` (`AndroidConfig::loopback`) | Từ chối địa chỉ non-loopback; token rỗng bị từ chối. | TÁI DÙNG kỷ luật: constructor ép ràng buộc địa chỉ, không để caller tự nhớ. Hub §5 dùng cùng kỷ luật cho bind address. |
| `android.rs:241-253` (`tokens_equal`) | So sánh không early-exit (XOR tích lũy). | TÁI DÙNG cho mọi so sánh secret/fingerprint/SAS trong pairing + hub. |
| `android.rs:332-404` (`PinnedVerifier`, `pinned_client_config`) | Verifier tin đúng 1 cert: SHA-256 DER == pin; không CA, không hostname; TLS 1.3 + 1.2, provider ring; `fingerprint_of_cert_der` ở `:407-413`. | TÁI DÙNG NGUYÊN VẸN cho phía client khi peer đã có TLS (Android shell, firmware ESP32). MỞ RỘNG: cần verifier phía server cho client cert pin — xem §5.2, quyết định D-DESIGN7-2. |
| `android.rs:421-472, 535-569, 1303-1310` | Session binding: TLS exporter RFC 5705, label `"mimint android v2 auth"`, `auth = hex(HMAC-SHA256(exporter, token))`, mode `bound`/`raw`; server tính expected từ transport, không từ request. | TÁI DÙNG Ý TƯỞNG binding (giá trị auth vô dụng ngoài session của nó) cho friends handshake §2.4: transcript binding + session key thay cho exporter (vì friends không dùng TLS). Label exporter của Android KHÔNG tái dùng cho friends (mỗi protocol một label riêng, tránh lẫn transcript). |
| `android.rs:1209-1227` (`fake_tls_config`), `:1316+` (`serve_connection`) | `rustls::ServerConfig` duy nhất trong cây, chỉ trong fake server test; cert/key từ fixture PEM commit sẵn (`tests/fixtures/android-test-*.pem`). | KHÔNG tái dùng làm production: đây là test helper. Hub TLS server (nếu có) là MỚI; sinh cert là việc chưa có công cụ — xem §5.2 (D-DESIGN7-2). |
| `docs/pairing.md` (F4-12) | Nguyên tắc: mắt + bàn phím, không default, pin là danh tính, token bound theo session, re-pair là thu hồi; CẤM pairing qua mạng ("pair request on the wire must never be added"); không QR, không discovery. | KẾ THỪA nguyên tắc mắt + bàn phím + pin là danh tính cho §3. Friends pairing CẦN trao đổi handshake qua mạng (SAS + transcript) — §3.1 phân biệt "pair request xin secret" (vẫn cấm) với "handshake xác thực lẫn nhau có SAS ngoài băng" (cho phép), đã chốt tại D-DESIGN7-1. |
| `crates/mimint-core/src/vault.rs:30-131` | `Vault` trait (`get` → `Zeroizing<String>`, không bao giờ log giá trị); `EnvVault`; `FileVault`: 1 file 1 secret, `0600` từ lần ghi đầu, mở bằng `O_NOFOLLOW`, kiểm tra quyền trên descriptor đã mở (chống TOCTOU), `create` từ chối ghi đè. Giới hạn ghi rõ: chống user khác, không chống malware cùng user. | TÁI DÙNG kỷ luật descriptor-check + `Zeroizing` + không log cho kho khóa thiết bị §3.2. MỞ RỘNG: cần kho byte (DER) + ghi khóa mới do app sinh — `Vault::get` chỉ đọc String và `FileVault::create` chỉ ghi text; kho khóa thiết bị là module MỚI mượn đúng kỷ luật này. |
| `crates/mimint-core/src/keychain.rs:48-56` | `KeychainVault` CHỈ ĐỌC (macOS), service `ai.mimint`, account = tên; không có API ghi; đọc có thể popup nên cấm chạy từ UI frame. | KHÔNG dùng để lưu khóa thiết bị trong v1 (không ghi được — xem §3.2, D-DESIGN7-3). Tái dùng ý tưởng "vault từng OS" ở mức kỷ luật, không ép API đọc-only vào việc ghi. |
| `crates/mimint-core/src/profile.rs:44-89, 116-199` | `ProfileEntry` (TOML, `deny_unknown_fields`) → `Profile` (đã validate) → `ProfileSet`; lỗi ngữ nghĩa fail-closed (drop profile + warning); trường: name, soul, chain, voice, skills, proactive, budget_usd, privacy, cost_cap_usd, avatar. Module doc ghi rõ: "Cross-body profile sync is Phase 7". | TÁI DÙNG NGUYÊN VẸN: record profile sync là `ProfileEntry` đã serialize; phía nhận chạy đúng `ProfileSet::resolve` (§4.5). Không sync key/backend secret — profile thiếu backend ở máy nhận thì drop như luật hiện có. |
| `crates/mimint-core/src/notebook.rs:66-76, 404-434, 494-729` | `Note { id, title, body, created_ms, updated_ms, owner_agent_id, shared }`; migration thêm cột idempotent; `add_for/get_for/search_for/update_for/delete_for` phân vùng theo agent; `Mutex<Connection>`; BM25 tự viết, không FTS. `id` là INTEGER tự tăng THEO DB — không ổn định liên body. | TÁI DÙNG phân vùng shared/private làm ranh giới sync: chỉ dòng `shared=true` mới được sync (§4.2). MỞ RỘNG: thêm cột `sync_id` ổn định liên body (§4.3) — migration duy nhất của Phase 7 lên DB hiện có. |
| `crates/mimint-core/src/audit.rs:41-58, 110-140` | `AuditRecord { ts_ms, turn_id, kind, tool, decision, detail, agent_id }`; `log` (system `agent-000`) / `log_for`; append-only JSONL, ghi hỏng thì fail-closed; hash-chain là TODO Phase 8 (P0-22), CHƯA có. | TÁI DÙNG: mọi sự kiện pairing/sync/hub/approve/handoff đều ghi qua `log_for` với `agent_id` của agent gây ra (hoặc `agent-000` cho sự kiện hệ thống). Attribution theo thiết bị ghi trong `detail` (§6.3) — không thêm trường first-class mới vào `AuditRecord` trong Phase 7 (D-DESIGN7-6). Audit log KHÔNG BAO GIỜ sync. |
| `crates/mimint-core/src/guards.rs:108-149, 375-396` | Binding phê duyệt là cặp `(agent_id, request_id)`; lệch một trong hai → deny `approval_id_mismatch`; `request_id` đơn điệu theo từng executor từ 1. | TÁI DÙNG + MỞ RỘNG: binding xuyên thiết bị thành bộ ba `(device_id, agent_id, request_id)` ở tầng vận chuyển quyết định (§6.2); executor local vẫn kiểm tra cặp cũ — kiểm tra device là lớp ngoài, không sửa ngữ nghĩa `GuardedExecutor`. |
| `crates/mimint-ui/src/approver.rs:69-71, 150-156` | Timeout thẻ mặc định 60 giây (`DEFAULT_APPROVAL_TIMEOUT`); `ApprovalJob` mang deadline/timeout; timeout = deny. | TÁI DÙNG: phê duyệt xuyên thiết bị giữ nguyên 60 giây (§6.2). Không có timeout riêng cho peer. |
| `crates/mimint-core/src/egress.rs:40-68, 120-151, 253-298` | `EgressPolicy` cố định lúc build client, không setter; `validate_url` trước mọi socket; Privacy ON mặc định, chỉ 3 dạng loopback (`127.0.0.1`, `localhost`, `::1`); `LoopbackGuardResolver` kiểm tra địa chỉ đã resolve; `GuardedHttpClient` là nơi duy nhất dựng reqwest client. | TÁI DÙNG KỶ LUẬT cho ranh giới sync §9: kiểm tra địa chỉ peer TRƯỚC khi nối, kiểm tra địa chỉ đã resolve (không tin chuỗi hostname). `GuardedHttpClient` không dùng cho friends (friends không phải HTTP) — kỷ luật được diễn đạt lại cho TCP peer, không ép HTTP client vào. |
| `crates/mimint-core/src/config.rs:368-410` | `FileConfig` với `deny_unknown_fields`; section mới phải khai báo tường minh; `ResolvedConfig` đã validate. | MỚI: thêm section `[friends]`, `[hub]`, `[sync]` theo đúng mẫu này (§12). Không có section thì mọi tính năng Phase 7 ở trạng thái tắt an toàn. |
| `crates/mimint-core/src/soul.rs:277-385` | `SoulStore::load/load_or_create/save` (save ghi history `soul-history.jsonl`); `SOUL_FORMAT_VERSION = 1`; tên file SOUL phải hợp lệ (`is_valid_soul_file_name`). | TÁI DÙNG: record SOUL sync là (tên file + body); phía nhận validate bằng `parse_soul_file` + tên file, ghi qua đường có history như `save` (§4.4). History KHÔNG sync. |
| `crates/mimint-core/src/skills.rs:241-352, 551-699` | `SkillDef::validate_shape/content_hash`; `SkillStore` file-based (`load_all/get/save/create_draft/import_file`); `promote/demote` có audit. | TÁI DÙNG: chỉ skill đã promote mới sync; phía nhận validate shape + hash trước khi `save` (§4.4). Draft/import chưa qua cổng không bao giờ sync. |
| `crates/mimint-core/src/mcp.rs:94-97, 165` | MCP client STDIO duy nhất; offer `2025-06-18`, chấp nhận `2025-06-18`/`2025-11-25`; từ chối `2026-07-28` stateless. | KHÔNG tái dùng trực tiếp cho MCP thiết bị ESP32 (§8.4): MCP thiết bị chạy trên kênh voice của firmware, version của nó (`2026-07-28`/`2024-11-05` theo GAP-CHECK) khác thế hệ client Phase 6. Hub nói MCP thiết bị bằng module MỚI, hẹp, chỉ đủ cho lệnh servo — không nối `McpManager` Phase 6 vào firmware. |
| `crates/mimint-control/src/actor.rs` | `BodyActor`: 1 thread sở hữu body, timeout action 30 giây, latch `broken` vĩnh viễn, `UnknownOutcome` khi timeout/panic. | TÁI DÙNG: handoff lease (§7) và hub driver (§5) đều đi qua actor hiện có — không có đường thứ hai lái body. Lease là lớp trên actor, không thay actor. |
| Không tồn tại trong cây | Không có store todos/plans (grep 0 hit cho `Todo`/`Plan` struct, `todos.json`/`plans.json`); không có listener production nào (chỉ fake server test); không có dep WebSocket/rcgen/uuid/base64 trực tiếp. | Todos/plans là record MỚI với schema tối thiểu (§4.6) — thiết kế định nghĩa, không giả vờ đã có. Hub listener, WS framing, sinh id đều MỚI, tự triển khai bằng std + crate đã có. |

### 0.5 Số lượng test baseline

Đếm thuộc tính `#[test]`/`#[tokio::test]` tại HEAD: **662**. CRITERIA G1 ghi "650 cũ + mới" — con số 650 là xấp xỉ, không phải số đếm chính xác tại HEAD. Cổng G1 trong bản này được hiểu là: toàn bộ test tại HEAD (662) + test mới đều xanh, 0 fail; nếu đếm bằng `cargo test` cho số khác (test bị ignore, test sinh động) thì số chạy thật mới là chuẩn, không phải 650. Cách hiểu này đã chốt (xem nhật ký độ lệch §14, mục 5); giai đoạn 2 đếm theo số chạy thật, không đếm sai thành 650.

### 0.6 Tuyên bố clean-room (D1) cho protocol friends/sync

Bản thiết kế này:

- KHÔNG dùng tên method, tên trường wire, giá trị frame, chuỗi label/salt, định dạng QR, hay số cổng nào của andee gốc. Mọi tên wire trong §2 (§2.2–§2.4) là tên mới do bản này đặt.
- KHÔNG đọc code andee trong lúc soạn; nguồn duy nhất về "cơ chế" là các báo cáo tổng hợp đã có (SYNTHESIS/GAP-CHECK), ở mức: khóa pinned + ECDH ephemeral + AEAD + mã 6 số + 2 mức trust. Từ mức ý tưởng đó, bản này tự thiết kế lại toàn bộ thứ tự handshake, envelope, và luật versioning.
- Tái dùng duy nhất các cơ chế của CHÍNH cây Mimint (bảng §0.4) + các nguyên tắc mật mã chuẩn công khai (ECDH, HKDF, AES-GCM, ECDSA — đều là API của `ring`, không phải thiết kế riêng của andee).
- Đường XiaoZhi (§8) dùng tài liệu hợp đồng công khai của firmware (OTA/WS/MQTT/MCP) và code MIT của `78/xiaozhi-esp32` ở mức giao diện đã ghi trong GAP-CHECK/Lane 2; firmware fork (nếu có) giữ attribution MIT. Không copy thiết kế wire của andee vào đường này.

---

## 1. Threat model (P7.0 mục 1)

### 1.1 Quyết định

Threat model của Phase 7, viết rõ ranh giới TRONG/NGOÀI phạm vi. Mọi cơ chế ở §2–§9 phải chỉ ra nó chống lại mục nào trong bảng; mục nào ngoài phạm vi thì tài liệu phải nói rõ, không ngụ ý đã chống.

| # | Kẻ tấn công / mối đe dọa | TRONG hay NGOÀI | Cơ chế chống (chỉ số §) |
|---|---|---|---|
| T1 | Kẻ nghe lén trên LAN (đọc được mọi gói tin giữa hai body) | TRONG | Mã hóa E2E mọi message sau handshake (§2.4); không plaintext nào mang dữ liệu user sau HELLO; HELLO chỉ mang public key + device id (không secret). |
| T2 | Kẻ giả mạo peer (tự xưng là body đã pair, nối tới hub/body) | TRONG | Pin khóa danh tính từ pairing (§3); handshake xác thực lẫn nhau bằng chữ ký trên transcript (§2.4); peer lạ bị từ chối trước khi trao đổi dữ liệu (§2.4, §5.2). |
| T3 | Kẻ đứng giữa MITM (chặn + sửa + chuyển tiếp handshake/sync) | TRONG | SAS 6 số ngoài băng + fingerprint ngoài băng (§3.3); chữ ký transcript ràng buộc toàn bộ handshake; AEAD tag phát hiện mọi sửa đổi ciphertext (§2.4). MITM không có SAS/fingerprint đúng thì handshake thất bại và audit ghi rõ. |
| T4 | Replay (gửi lại message/quyết định/gói handoff cũ) | TRONG | Số thứ tự + nonce đơn điệu theo session (§2.3); binding quyết định (§6.2); lease có TTL + version (§7.3); quyết định phê duyệt dùng 1 lần. |
| T5 | Downgrade (ép về protocol cũ hơn / plaintext / cipher yếu hơn) | TRONG | Version nằm trong transcript đã ký (§2.4); không có mode plaintext cho friends (khác Android `TlsMode::Plain` chỉ dành cho fake/debug loopback); một cipher suite duy nhất, không đàm phán (§2.2). |
| T6 | Peer đã pair nhưng bị chiếm (thiết bị pair rồi bị kẻ khác cầm) | TRONG, có giới hạn | Thu hồi (§3.6) cắt mọi kết nối sau đó; guest bị sandbox (§3.5); sync không bao giờ mang secret nên peer bị chiếm không lấy được vault/key/history (§4.2). GIỚI HẠN: peer bị chiếm TRƯỚC khi thu hồi vẫn đọc được dữ liệu sync đã nhận — không có forward secrecy cho dữ liệu đã sync xong; tài liệu phải nói rõ. |
| T7 | Guest vượt quyền (guest đọc private, pair tiếp, phê duyệt hộ) | TRONG | Allow-list phía server (§3.5, §5.3); guest không sync, không approve, không handoff-nhận, không pair tiếp; mọi vi phạm deny + audit. |
| T8 | Kẻ tấn công mạng gây DoS nhẹ (mở nhiều kết nối, gửi frame lớn) | TRONG, giới hạn | Giới hạn frame/batch (§2.2), timeout handshake, giới hạn kết nối đồng thời (§5.4), lockout pairing (§3.4). Không chống được DoS băng thông lớn — ngoài phạm vi. |
| T9 | Server trung gian độc hại / relay rò rỉ | KHÔNG TỒN TẠI | Không có relay, không có cloud account, không có server trung gian nào trong thiết kế. Mọi kết nối là trực tiếp peer-to-peer trên LAN/tailnet. Đây là tính chất kiến trúc, không phải cơ chế. |
| T10 | Malware chạy cùng user trên cùng máy | NGOÀI (đã chốt từ Phase 2) | Không cơ chế nào trong Phase 7 chống được malware cùng user (nó đọc được file 0600, bộ nhớ tiến trình, màn hình). Vault doc `vault.rs` đã ghi rõ giới hạn này; thiết kế này giữ nguyên, không tuyên bố hơn. |
| T11 | Kẻ có quyền vật lý lên thiết bị (cầm máy đã mở khóa, dump flash ESP32) | NGOÀI, trừ thu hồi | Không chống dump flash/khóa trên ESP32 đường XiaoZhi (bí mật NVS plaintext — GAP-CHECK M5). Biện pháp duy nhất: thu hồi ở hub + xoay key (§3.6) sau khi phát hiện. Firmware v2 có NVS encryption là việc tương lai (§13), không phải Phase 7. |
| T12 | Lỗi của chính user (pair nhầm thiết bị, bấm Confirm khi SAS lệch) | TRONG, giảm thiểu | SAS + fingerprint hiển thị rõ, so sánh bắt buộc (§3.3); thu hồi 1 lần bấm (§3.6); UI không bao giờ tự Confirm. Không chống được user cố tình bỏ qua cảnh báo — ghi rõ trong docs. |
| T13 | Kẻ tấn công đường XiaoZhi (MITM OTA, sửa audio UDP) | TRONG, có giới hạn | OTA nhà chỉ trả URL an toàn, trừ ngoại lệ lab duy nhất (§8.2); ép WebSocket, không dùng MQTT/UDP audio trong v1 (§8.2); modem EC801E/ML307 không được tuyên bố xác thực (§8.1). GIỚI HẠN: firmware XiaoZhi không pin cert, không ép scheme — an toàn của đường này PHỤ THUỘC cấu hình server nhà + LAN tin cậy; tài liệu + UI phải ghi đúng giới hạn này, không tuyên bố "đã mã hóa + xác thực" chung chung (Lane 2 §10). |

### 1.2 Giả định tin cậy (assumptions)

1. **User là trust root.** Pairing ceremony (§3) tin rằng user so sánh SAS/fingerprint thật và chỉ Confirm khi khớp. Không có cơ chế nào thay được bước này.
2. **LAN/tailnet là kênh truyền không tin cậy** (T1–T5) nhưng **không phải kẻ thù toàn năng**: kẻ tấn công đọc/sửa được gói tin, nhưng không đọc được màn hình hai thiết bị cùng lúc để lấy SAS, không chiếm được cả hai thiết bị cùng lúc trong lúc pairing.
3. **Mỗi body tin chính nó.** OS + tiến trình Mimint trên body không bị xâm nhập (T10 ngoài phạm vi). Khóa thiết bị lưu 0600 là đủ trong giả định này.
4. **Đồng hồ các body gần đúng** (lệch dưới vài phút). Hết hạn SAS/lease/timeout dùng đồng hồ local + TTL, không dùng timestamp tuyệt đối của peer làm chuẩn duy nhất; lệch giờ lớn thì pairing/lease fail-closed (hết hạn sớm), không fail-open.
5. **Không tin DNS.** Tên peer không được tin; địa chỉ peer phải kiểm tra sau khi resolve (§9.3), như kỷ luật `LoopbackGuardResolver`.

### 1.3 Những gì KHÔNG được tuyên bố

- Không tuyên bố "E2E" cho đường XiaoZhi (server nhà nhìn thấy plaintext voice; firmware không có E2E — GAP-CHECK §2.3). Chỉ friends/sync v2 mới là E2E.
- Không tuyên bố forward secrecy cho dữ liệu đã lưu trên peer (chỉ cho session đang chạy: khóa phiên ephemeral, xóa sau session).
- Không tuyên bố chống malware cùng user, chống dump flash, chống DoS băng thông.
- Không tuyên bố hub "an toàn" khi user bật LAN mà không pair: LAN listen YÊU CẦU pairing đã bật (§5.1); cấu hình bật LAN mà chưa pair thiết bị nào thì hub từ chối mọi kết nối điều khiển và ghi rõ trạng thái đó trong UI.

### 1.4 Kiểm chứng

- `threat_model_documented`: test cổng grep — tài liệu vận hành Phase 7 liệt kê đủ T1–T13 với đúng nhãn TRONG/NGOÀI và giới hạn T6/T13; không có câu nào tuyên bố chống malware cùng user hay E2E cho đường XiaoZhi.
- Các test tấn công cụ thể nằm ở §2.6, §3.8, §5.5, §6.4, §7.4 (mỗi test đặt tên đúng mối đe dọa nó khóa).

---

## 2. Protocol v2 friends/sync (P7.0 mục 2) — MỚI, viết mới hoàn toàn

### 2.1 Quyết định tổng thể

- **Tên gọi:** "friends protocol v2" trong bản này là protocol MỚI cho peer-to-peer giữa các body Mimint. Nó KHÔNG phải protocol v2 của Android body (§0.4) — hai protocol khác nhau, khác method, khác envelope, khác cổng; chỉ chung số version `2` và hình thức framing u32 + JSON (hình thức chung của cây). Không có khả năng nối chéo: client friends nối tới Android shell (hoặc ngược lại) phải thất bại ở bước version/kind đầu tiên, không bao giờ hiểu nhầm message.
- **Transport:** TCP trực tiếp, không HTTP, không WebSocket (trừ đường XiaoZhi §8 dùng WS riêng). Không relay, không server trung gian. Địa chỉ peer nhập tay ở v1 (§4.7).
- **Một cipher suite duy nhất, không đàm phán:** P-256 ECDH ephemeral + HKDF-SHA256 + AES-256-GCM + ECDSA P-256 (SHA-256, ASN.1) cho xác thực. Không có cipher nào khác, không có fallback. Lý do: đàm phán là bề mặt downgrade (T5); một cipher suite duy nhất thì code kiểm chứng được và probe §0.3 đã chứng minh đủ.
- **Mọi message sau handshake đều mã hóa AEAD.** Không có message plaintext nào mang dữ liệu user. Hai message đầu handshake (HELLO) là plaintext nhưng chỉ mang public key + device id + version — không secret, không dữ liệu user.
- **Xác thực lẫn nhau, cả hai chiều.** Không có chiều "client tin server mù": cả hai bên đều ký transcript bằng khóa danh tính của mình và đều kiểm tra pin của bên kia trước khi gửi dữ liệu (§2.4).
- **Phạm vi phiên pairwise.** Mỗi session là pairwise giữa đúng 2 body; một body giữ tối đa 4 session đồng thời (§5.4); không có group session trong Phase 7.

### 2.2 Envelope và framing + quy ước chiều duy nhất

**Quy ước chiều duy nhất (áp dụng cho toàn bộ §2):** trong một cặp hai body, bên có `device_id` nhỏ hơn (so sánh byte) là **A**, bên còn lại là **B**. Quy ước này dùng cho CẢ nonce AEAD dưới đây LẪN chuỗi HKDF info/salt ở §2.4. Không có quy ước chiều thứ hai nào. Nhãn vai trò TCP (bên khởi tạo / bên nghe) chỉ dùng trong văn xuôi để chỉ ai nối tới ai, không bao giờ xuất hiện trong bất kỳ chuỗi crypto hay trường wire nào.

Framing (tái dùng hình thức của Android protocol, nội dung mới):

```text
[u32 big-endian: độ dài JSON header][JSON header][payload bytes]
```

- JSON header ≤ 64 KiB (nhỏ hơn 1 MiB của Android protocol: header friends không mang screenshot; 64 KiB là đủ cho mọi header + chữ ký). Payload ≤ 4 MiB cho một frame sync (batch), ≤ 256 KiB cho một record (xem §4.7). Vượt giới hạn → lỗi protocol, đóng kết nối, audit.
- Header của message đã mã hóa (sau handshake):

```json
{
  "v": 2,
  "kind": "sync_push",
  "from": "<device_id 32 hex>",
  "seq": 41,
  "nonce": "<hex 12 byte>",
  "payload_len": 1832
}
```

Payload là ciphertext AES-256-GCM của plaintext JSON (định dạng plaintext theo từng `kind`, xem §2.5). AAD của AEAD = JSON header đã canonical hóa theo thứ tự trường cố định như trên (encode lại từ các trường đã parse, không dùng byte thô nhận được — tránh malleability qua khoảng trắng/thứ tự khóa JSON). Nonce KHÔNG BAO GIỜ tái dùng trong một session: nonce = 4 byte cố định theo chiều (`A->B` cho chiều từ A sang B, `B->A` cho chiều ngược lại, theo quy ước device_id ở đầu § này) + 8 byte counter big-endian đơn điệu theo chiều, bắt đầu từ 0. Counter chạm 2^63 → bắt buộc rekey (§2.4) trước message tiếp theo; thực tế rekey xảy ra sớm hơn nhiều theo §2.4.
- `seq` là số thứ tự theo chiều, bắt đầu từ 1 sau handshake, tăng 1 mỗi message. Bên nhận từ chối mọi message có `seq` không phải `seq_kỳ_vọng` (không có khoảng trống, không có trùng lặp) → chống replay + chống xóa message âm thầm (T4). Mất đồng bộ seq → đóng session, bắt tay lại; không có cơ chế "bỏ qua seq".
- `kind` là allow-list đóng, liệt kê ở §2.5. Kind lạ → lỗi protocol, đóng kết nối. Không có kind mở rộng tự do, không có trường custom.
- `v` phải là `2`; khác → lỗi version, đóng kết nối. Version nằm trong transcript đã ký (§2.4) nên không downgrade được.

Lý do chọn JSON header + payload nhị phân (thay vì toàn JSON hoặc toàn nhị phân): JSON header cho phép kiểm tra version/kind/seq/nonce trước khi giải mã (fail nhanh, log được kind mà không log được nội dung); payload nhị phân giữ ciphertext đúng dạng AEAD (không base64 phình to, không lẫn encoding). Đây là cùng lý do Android protocol dùng hình thức này — tái dùng lý do, không copy thiết kế wire của andee.

Phương án đã loại: (a) **Toàn bộ message là một JSON gồm ciphertext base64** — loại vì phình ~33%, và vì không có base64 dep trực tiếp trong cây (phải tự viết encoder/decoder, thêm bề mặt lỗi). (b) **Framing nhị phân thuần với struct cố định** — loại vì khó mở rộng kind và khó debug bằng log; JSON header đủ nhanh ở quy mô này (header ≤ 64 KiB, xem ngân sách §10). (c) **Dùng TLS thay cho AEAD tự làm** — loại cho friends/sync vì: cây không có công cụ sinh cert (§5.2), TLS không cho kiểm soát seq/nonce theo ý thiết kế, và pin khóa P-256 thô + AEAD đạt cùng tính chất với ít moving part hơn. TLS vẫn dùng ở nơi peer đã có TLS (Android, ESP32) — xem §5.2.

### 2.3 Versioning bản ghi (tóm tắt — chi tiết ở §4.3, thuật toán phát hiện concurrent ở §4.4)

Mỗi record sync mang version là cặp **(Lamport clock u64, device_id)**:

- Lamport clock: mỗi body giữ một counter đơn điệu; khi tạo/sửa record, counter tăng 1 và gán vào record; khi nhận record từ peer, counter local = max(local, remote) + 1. Không dùng đồng hồ wall-clock làm version (lệch giờ không được làm mất dữ liệu).
- **Vai trò duy nhất của Lamport là sắp xếp hiển thị và giữ tính đơn điệu.** Cặp (Lamport, device_id) cho một thứ tự toàn phần để hiển thị các version, và để kiểm tra "đã thấy version này chưa" khi kết hợp với sync state (§4.4). **Lamport KHÔNG BAO GIỜ quyết định ghi đè, cập nhật, hay chọn bản thắng.** Không có quy tắc nào trong bản này suy ra "bản có Lamport lớn hơn thì thắng" hay "bản có Lamport lớn hơn thì mới hơn và được áp dụng". Việc một version remote được áp dụng, bị bỏ qua, hay tạo conflict chỉ do thuật toán §4.4 quyết định, dựa trên quan hệ nhân quả ghi trong sync state và trường `based_on` của record — không do so sánh số Lamport.
- Vector clock đã được cân nhắc và LOẠI: với ≤ 4 body, vector clock cho thông tin quan hệ trước-sau chính xác hơn, nhưng tốn kích thước theo số device, phức tạp khi device bị thu hồi/xoay key (vector phải cắt entry), và không cần thiết vì luật của bản này là "concurrent → conflict cho user chọn". Quyết định giữ Lamport + device_id đã chốt tại D-DESIGN7-4, kèm thuật toán §4.4 bắt buộc.

### 2.4 Thứ tự handshake

Handshake gồm 5 bước, theo đúng thứ tự này. Bất kỳ bước nào thất bại → đóng kết nối ngay, không gửi thêm gì, audit ghi rõ bước + lý do (không ghi secret). Timeout toàn bộ handshake: 10 giây (cùng thang IO timeout của Android protocol; giới hạn chống DoS ở §5.4: tối đa 8 kết nối đang chờ handshake, timeout handshake 10 giây, cap frame/batch ở §2.2). Thứ tự này là thiết kế mới của bản này; tên message là tên mới.

Hai bên: bên khởi tạo nối TCP tới bên nghe. Cả hai đều có khóa danh tính ECDSA P-256 dài hạn (khóa private trong kho khóa thiết bị §3.2, public key đã pin từ lúc pair §3). Cả hai đều sinh khóa ECDH ephemeral mới cho session này. Trong sơ đồ dưới đây, "bên khởi tạo" là bên mở TCP, "bên nghe" là bên chấp nhận TCP; hai vai trò này chỉ dùng để xác định thứ tự gửi message, không liên quan tới quy ước chiều A/B của §2.2 (A/B xác định bằng device_id, độc lập với ai khởi tạo).

```text
Bên khởi tạo → bên nghe: HELLO { v:2, kind:"hello", device_id, identity_pub (65B hex),
                                 ecdh_pub (65B hex), nonce (16B random hex) }
Bên nghe → bên khởi tạo: HELLO { v:2, kind:"hello", device_id, identity_pub (65B hex),
                                 ecdh_pub (65B hex), nonce (16B random hex) }
Bên khởi tạo → bên nghe: AUTH  { v:2, kind:"auth", device_id,
                                 sig: ECDSA_sign(khóa_danh_tính, transcript_hash) }
Bên nghe → bên khởi tạo: AUTH  { v:2, kind:"auth", device_id,
                                 sig: ECDSA_sign(khóa_danh_tính, transcript_hash) }
(sau khi cả hai AUTH đều verify xong: kênh mã hóa mở; message đầu tiên mỗi chiều
 là SESSION_CONFIRM đã mã hóa, mang device_id + trust level đã pair + protocol
 version — bên nhận kiểm tra khớp với registry trước khi xử lý kind khác)
```

Quy tắc chi tiết:

1. **Kiểm tra pin TRƯỚC khi ký.** Mỗi bên, khi nhận HELLO, kiểm tra ngay: `device_id` có trong registry không, `identity_pub` có khớp đúng public key đã pin cho device đó không, device có bị thu hồi không (§3.6). Không khớp → đóng kết nối, KHÔNG gửi AUTH (không ký cho peer lạ — chữ ký cho transcript của peer lạ là bề mặt lạm dụng). Đây là điểm "peer lạ bị từ chối trước khi trao đổi dữ liệu" của CRITERIA P7.2.
2. **Transcript.** `transcript` = nối có độ dài của: version (u32) + byte JSON của HELLO do bên khởi tạo gửi + byte JSON của HELLO do bên nghe gửi (đúng byte JSON đã gửi/nhận, không parse lại). Thứ tự nối transcript là thứ tự theo VAI TRÒ TCP (HELLO của bên khởi tạo trước, HELLO của bên nghe sau) và cố định như vậy cho cả hai bên khi tính — thứ tự này KHÔNG phải thứ tự A/B, và không được lẫn với thứ tự A/B dùng cho khóa ở bước 4. `transcript_hash` = SHA-256(transcript). Version, device id, cả hai public key ephemeral, cả hai nonce đều nằm trong transcript → MITM sửa bất kỳ trường nào cũng làm chữ ký sai (T3/T5).
3. **Chữ ký.** ECDSA P-256 với SHA-256, encoding ASN.1 DER (thuật toán `ECDSA_P256_SHA256_ASN1_SIGNING` của ring — đã xác nhận có trong ring 0.17.14, probe c2). Verify bằng public key ĐÃ PIN (không phải public key vừa nhận — public key vừa nhận chỉ được dùng sau khi đã khớp pin ở bước 1; verify phải dùng bản pin trong registry để tránh lẫn lộn). Chữ ký sai → đóng kết nối.
4. **Khóa phiên.** Sau khi cả hai HELLO đã trao đổi (trước AUTH cũng được, nhưng chỉ DÙNG sau khi AUTH verify xong): ECDH ephemeral hai chiều → shared secret → HKDF-SHA256. Đặt `nonce_A` là nonce trong HELLO của bên có device_id nhỏ hơn (A), `nonce_B` là nonce trong HELLO của bên còn lại (B), theo quy ước duy nhất ở §2.2. Khi đó: salt = `SHA-256(nonce_A || nonce_B)`; info theo chiều là `"mimint friends v2 session A->B"` cho khóa chiều A→B và `"mimint friends v2 session B->A"` cho khóa chiều B→A. Hai khóa 32 byte độc lập theo chiều. Shared secret ephemeral bị xóa (zeroize) ngay sau khi HKDF xong; khóa ephemeral private cũng xóa sau khi agree xong. Không lưu khóa phiên xuống đĩa, không log. Mọi thao tác mật mã đều qua API của `ring` (so sánh secret/fingerprint dùng phép so sánh không early-exit như `tokens_equal`); cấm ghi secret, khóa, nonce, hay SAS vào log hay error message.
5. **SESSION_CONFIRM.** Message mã hóa đầu tiên mỗi chiều, plaintext: `{ device_id, paired_trust:"owner"|"guest", proto:2 }`. Bên nhận kiểm tra: device_id khớp HELLO + registry, trust khớp registry (peer không tự nâng trust của mình — trust lấy từ registry local, claim trong message chỉ để kiểm tra nhất quán; lệch thì đóng kết nối). Sau CONFIRM, các kind khác mới được xử lý.
6. **Rekey (đặc tả đầy đủ).** Khi một chiều đã gửi 1.000.000 message, hoặc 1 GiB ciphertext, hoặc session đã sống 24 giờ (điều kiện nào đến trước): bên khởi tạo của session bắt đầu rekey. Bộ đếm rekey `n` thuộc về session, bắt đầu từ 0, tăng 1 sau mỗi lần rekey thành công, reset khi session mới bắt đầu; lần rekey đầu tiên của session dùng `n = 1`.
   - Nội dung `rekey` (đã mã hóa bằng khóa cũ): `{ ecdh_pub_new (65B hex), nonce_new (16B random hex) }` do bên bắt đầu rekey sinh mới. Nội dung `rekey_ack` (đã mã hóa bằng khóa cũ): `{ ecdh_pub_new (65B hex), nonce_new (16B random hex) }` do bên còn lại sinh mới.
   - Khóa mới suy ra bằng HKDF-SHA256 từ shared secret ECDH mới, với salt = `SHA-256(nonce_new_A || nonce_new_B)` (nonce mới sắp theo quy ước A/B của §2.2) và info có hậu tố rekey: `"mimint friends v2 session A->B/rekey/<n>"` và `"mimint friends v2 session B->A/rekey/<n>"`.
   - Thứ tự chuyển khóa, từng bước: (a) bên bắt đầu gửi `rekey` bằng khóa cũ, tiếp tục gửi bằng khóa cũ cho tới khi nhận `rekey_ack`; (b) bên nhận `rekey` sinh cặp mới, suy ra khóa mới, gửi `rekey_ack` bằng khóa cũ, rồi chuyển sang GỬI bằng khóa mới ngay sau khi gửi ack; (c) bên bắt đầu nhận `rekey_ack`, suy ra khóa mới, chuyển sang GỬI bằng khóa mới; (d) mỗi bên GIỮ khóa cũ để NHẬN cho tới khi nhận được message đầu tiên giải mã được bằng khóa mới (hoặc hết timeout 10 giây kể từ lúc chuyển gửi), rồi zeroize khóa cũ ngay. Seq và nonce counter của mỗi chiều reset về 0 (seq bắt đầu lại từ 1, counter nonce từ 0) tại thời điểm bên đó chuyển sang gửi bằng khóa mới; bản thân `rekey`/`rekey_ack` dùng khóa cũ với counter đang chạy.
   - Message đang bay trong lúc rekey: message mã hóa bằng khóa cũ mà tới trong lúc bên nhận vẫn giữ khóa cũ thì được nhận bình thường (thử khóa mới trước, khóa cũ sau; message nào giải mã được bằng đúng một khóa thì xử lý, seq kiểm tra theo khóa tương ứng). Sau khi một bên đã zeroize khóa cũ, message khóa cũ tới sẽ fail AEAD → đóng session như F10 (§2.7); bên gửi vì vậy KHÔNG BAO GIỜ được gửi bằng khóa cũ sau khi đã chuyển sang gửi bằng khóa mới. Không có hàng đợi hay phát lại message nào trong lúc rekey: message gửi đi là message đã gửi, bên nhận xử lý theo khóa còn giữ.
   - Thất bại → đóng session + bắt tay lại: timeout chờ `rekey_ack` (10 giây), ECDH/nonce mới không hợp lệ, hay AEAD fail trong lúc chuyển khóa đều dẫn tới đóng session, zeroize cả khóa cũ lẫn khóa mới, và bắt tay lại từ đầu nếu còn cần nối. Không có "quay lại khóa cũ" sau khi rekey đã bắt đầu thất bại.
   - Rekey không cần chữ ký mới: kênh cũ đã xác thực + `rekey`/`rekey_ack` nằm trong AEAD của kênh cũ, nên bên nhận biết chắc gói rekey đến từ đúng peer đã xác thực.
7. **Đóng session.** Đóng TCP là đóng session; không có message "bye" mang ngữ nghĩa bảo mật. Khóa phiên bị zeroize khi session kết thúc vì bất kỳ lý do gì. Không resume session (không có session ticket, không có PSK): nối lại là bắt tay lại từ đầu. Lý do: resume là bề mặt replay/downgrade và không cần thiết khi handshake rẻ (§10: crypto < 1 ms, còn lại là RTT).

**Test vector cố định cho suy ra khóa theo chiều.** Vector dưới đây do thiết kế đặt ra và test `friends_key_derivation_test_vector` (§2.6) khóa đúng từng giá trị. Shared secret ECDH là input minh họa cố định (một shared secret thật phụ thuộc vào keypair ephemeral, không cố định được); salt và hai khóa chiều được tính ĐÚNG theo HKDF-SHA256 (RFC 5869) từ các input đó, không phải số bịa.

| Trường | Giá trị (hex) |
|---|---|
| `device_id` của A (nhỏ hơn) | `00112233445566778899aabbccddeeff` |
| `device_id` của B (lớn hơn) | `ffeeddccbbaa99887766554433221100` |
| `nonce_A` (16 byte) | `a0a1a2a3a4a5a6a7a8a9aaabacadaeaf` |
| `nonce_B` (16 byte) | `b0b1b2b3b4b5b6b7b8b9babbbcbdbebf` |
| Shared secret ECDH (input minh họa, 32 byte) | `c0c1c2c3c4c5c6c7c8c9cacbcccdcecfd0d1d2d3d4d5d6d7d8d9dadbdcdddedf` |
| salt = SHA-256(`nonce_A \|\| nonce_B`) | `00e988677eecf94c0bb9233371c7c0d6f4db8ebdcdecb7c5ebaa666f17249227` |
| PRK = HKDF-Extract(salt, shared secret) | `86b81109d3268e633aaf699b5381c73d3822fe2806eeee14b77312bee8555ee9` |
| Khóa chiều A→B, info `"mimint friends v2 session A->B"` | `7e70e8ce4edcfb747dbb1c6443359d71e7faa3e5146eb03fce79f768338d2bb9` |
| Khóa chiều B→A, info `"mimint friends v2 session B->A"` | `377441c9d277f30a2a7fd8106a64da399836de9bf64ed92fa778b2eac5030f95` |

Phương án đã loại: (a) **Noise IK hoặc Noise XX** — loại vì không có dep Noise trong cây và tự triển khai Noise là việc lớn, dễ sai; handshake 5 bước trên đạt cùng tính chất cần thiết (xác thực lẫn nhau, forward secrecy, transcript binding) bằng API ring đã chứng minh. (b) **PSK từ pairing (không ECDH ephemeral)** — loại vì mất forward secrecy: lộ PSK thì lộ toàn bộ session cũ. (c) **Chỉ xác thực một chiều (hub tin client)** — loại vì vi phạm T2 cả hai chiều; peer giả mạo hub cũng nguy hiểm như hub giả mạo peer (đều đọc được sync). (d) **SAS trong mọi handshake** — loại vì SAS chỉ cần lúc pairing (§3); handshake thường ngày dùng pin + chữ ký, không làm phiền user.

### 2.5 Các kind message (allow-list đóng)

Bảng dưới đây là TOÀN BỘ kind của friends protocol. Kind nào không có trong bảng thì bị từ chối. Các kind dùng TRƯỚC khi `SESSION_CONFIRM` hoàn tất là: `hello`, `auth` và `handshake_error` (trong handshake §2.4), cùng các kind ceremony pairing `pair_request`, `pair_response`, `pair_confirm`, `pair_accept`, `pairing_error` (chạy trên một kết nối ceremony riêng theo §3.3, không nằm trong session thường). Mọi kind còn lại trong bảng CHỈ được xử lý sau khi `SESSION_CONFIRM` đã trao đổi xong cả hai chiều; kind đó tới sớm hơn thì bị từ chối như sai thứ tự (F7, §2.7).

| Kind | Giai đoạn | Chiều | Plaintext mang gì | § chi tiết |
|---|---|---|---|---|
| `hello` | Handshake, trước auth | Cả hai | device_id + identity_pub + ecdh_pub + nonce (plaintext, không secret) | §2.4 |
| `auth` | Handshake, trước auth xong | Cả hai | device_id + chữ ký ECDSA trên transcript_hash | §2.4 |
| `handshake_error` | Handshake, khi thất bại | Bên phát hiện → bên kia | `{ code: "version_mismatch" \| "handshake_failed" }` — 1 message lỗi plaintext, không dữ liệu, không phân biệt nguyên nhân (trừ code version) | §2.7 |
| `session_confirm` | Đầu kênh mã hóa | Cả hai | device_id + trust + proto (bước 5 §2.4) | §2.4 |
| `rekey` / `rekey_ack` | Sau confirm | Bên bắt đầu → bên kia / ngược lại | ECDH ephemeral mới + nonce mới (mã hóa bằng khóa cũ) | §2.4 bước 6 |
| `pair_request` / `pair_response` | Ceremony pairing | Bên xin pair → bên nghe / ngược lại | Public key + ECDH ephemeral + nonce + mức trust xin cấp (xem §3.3). Ngoài ceremony thì bị từ chối. | §3.3 |
| `pair_confirm` | Ceremony pairing | Bên nghe → bên xin pair | Chữ ký của bên nghe trên transcript_hash của ceremony + mức trust bên nghe cấp cho bên xin pair. Chỉ gửi sau khi user phía bên nghe đã xác nhận (SAS + fingerprint + trust). | §3.3 |
| `pair_accept` | Ceremony pairing | Bên xin pair → bên nghe | Chữ ký của bên xin pair trên transcript_hash của ceremony + mức trust bên xin pair cấp cho bên nghe. Chỉ gửi sau khi user phía bên xin pair đã xác nhận riêng. Bên xin pair lưu pin của bên nghe khi gửi message này. | §3.3 |
| `pairing_error` | Ceremony pairing, khi thất bại | Bên phát hiện → bên kia | Lỗi chung, không nói sai ở đâu (không mang SAS, không mang lý do chi tiết). Audit phía gửi ghi lý do thật. | §3.3, §3.4 |
| `sync_offer` / `sync_request` / `sync_push` / `sync_ack` | Sau confirm | Cả hai | Version state, yêu cầu record, record đã mã hóa, xác nhận | §4.7 |
| `approval_card` / `approval_decision` | Sau confirm | Hub → peer / peer → hub | Thẻ phê duyệt đang chờ / quyết định | §6.2 |
| `handoff_offer` / `handoff_accept` / `handoff_reject` | Sau confirm | Gửi → nhận | Gói handoff đã ký, chấp nhận/từ chối | §7.3 |
| `handoff_complete` | Sau confirm | Bên nhận → bên gửi | Thông báo bên nhận đã xong task (hoặc đã dừng task) + `handoff_id` + `lease_id`; lease được giải phóng khi bên gửi nhận message này hoặc khi hết TTL. Thông báo, không bắt buộc cho tính đúng đắn của lease (TTL vẫn là chốt chặn cuối). | §7.2 |
| `ping` / `pong` | Sau confirm | Cả hai | Nonce kiểm tra sống, không dữ liệu | §5.4 (health) |
| `revoke_notice` | Sau confirm | Một chiều | Thông báo "key của tôi đã xoay" (best-effort, không tin mù — xem §3.6) | §3.6 |

Không có kind nào mang: secret/vault/key, history chat, audit log, trajectory quarantine (§4.2). Serializer của sync (§4.2) ép allow-list record kind; deserializer từ chối mọi record kind ngoài allow-list, kể cả khi peer đã pair (peer bị chiếm cũng không nhét được record lạ vào DB). Các chuỗi `lease_busy`, `forbidden_role`, `unknown_method`, `sas_mismatch` trong bản này là mã lý do trong payload/audit, không phải kind — chúng chỉ xuất hiện bên trong kind đã liệt kê ở trên (`handoff_reject` mang lý do `lease_busy`; phản hồi hub mang mã `forbidden_role`).

### 2.6 Kiểm chứng (protocol)

- `friends_handshake_happy_path`: 2 instance thật trên vega (2 data dir riêng, đã pair) bắt tay thành công, trao đổi 1 message mã hóa mỗi chiều, nội dung đúng.
- `friends_unknown_peer_rejected_before_data`: peer lạ (không trong registry) nối tới → bị từ chối sau HELLO, không AUTH nào được gửi, không dữ liệu nào trao đổi, audit ghi `unknown_peer`.
- `friends_wrong_pin_rejected`: peer mang đúng device_id nhưng public key khác pin → từ chối.
- `friends_revoked_peer_rejected`: peer đã revoke → từ chối như peer lạ (không phân biệt trong phản hồi — tránh rò rỉ trạng thái registry).
- `friends_tampered_hello_fails`: sửa 1 byte trong HELLO khi chuyển tiếp (MITM giả) → chữ ký transcript sai → handshake thất bại.
- `friends_replay_fails`: gửi lại một message đã mã hóa cũ trong session mới → seq/nonce/khóa sai → từ chối; gửi lại trong cùng session → seq trùng → đóng session.
- `friends_downgrade_fails`: HELLO với `v:1` → lỗi version; HELLO với version đúng nhưng transcript bị sửa version → chữ ký sai.
- `friends_wrong_key_cannot_read`: test giải mã payload sync bằng khóa sai → fail (bản protocol của probe ring §0.3, chạy trên code sản phẩm).
- `friends_key_derivation_test_vector`: với đúng input của test vector §2.4 (device_id, nonce_A/nonce_B, shared secret minh họa), code suy ra khóa phải cho đúng salt, PRK và hai khóa chiều trong bảng — từng byte.
- `friends_rekey_works`: ép rekey bằng ngưỡng thấp trong test → sau rekey, message vẫn qua, khóa cũ không đọc được message mới; bộ đếm `n` tăng và info mang đúng hậu tố `/rekey/<n>`.
- `friends_rekey_failure_closes_session`: rekey_ack không tới trong timeout (hoặc ECDH/nonce mới không hợp lệ) → session đóng, khóa cũ và khóa mới đều bị zeroize, bắt tay lại từ đầu thì nối được.
- `friends_rekey_old_key_rejected_after_switch`: sau khi cả hai bên đã chuyển sang khóa mới và zeroize khóa cũ, message mã hóa bằng khóa cũ gửi tới → AEAD fail → session đóng; message đang bay bằng khóa cũ trong cửa sổ giữ khóa cũ vẫn được nhận.
- `friends_no_plaintext_data`: bắt gói loopback trong test 2 instance → không message nào sau HELLO chứa plaintext của dữ liệu test (kiểm tra bằng cách tìm chuỗi sentinel trong luồng byte thô).
- `friends_cross_protocol_refused`: client friends nối tới Android fake server (và ngược lại) → thất bại ở bước đầu, không hiểu nhầm.

### 2.7 Bảng failure-mode của handshake (mọi đường thất bại đều đóng + audit)

Mỗi hàng là một ca mà giai đoạn 2 phải xử lý đúng; "phản hồi" là thứ duy nhất bên kia được thấy (không rò rỉ thêm thông tin nào).

| # | Tình huống | Bên phát hiện | Hành động | Phản hồi cho bên kia | Audit kind |
|---|---|---|---|---|---|
| F1 | TCP nối được nhưng không gửi gì trong 10 giây | Bên nghe | Đóng kết nối | (không có) | `handshake_timeout` |
| F2 | HELLO sai version | Bên nhận | Đóng ngay | `handshake_error` code `version_mismatch` (1 message lỗi plaintext, không dữ liệu) | `handshake_version_mismatch` |
| F3 | HELLO mang device_id không trong registry | Bên nhận | Đóng ngay, không gửi AUTH | `handshake_error` code `handshake_failed` | `unknown_peer` |
| F4 | HELLO mang device_id có trong registry nhưng public key khác pin | Bên nhận | Đóng ngay, không gửi AUTH | `handshake_error` code `handshake_failed` (giống F3 — không phân biệt lạ/revoke/sai pin) | `pin_mismatch` |
| F5 | Device đã revoke | Bên nhận | Đóng ngay | `handshake_error` code `handshake_failed` (giống F3) | `revoked_peer_attempt` |
| F6 | AUTH chữ ký sai | Bên nhận | Đóng ngay | `handshake_error` code `handshake_failed` | `auth_signature_invalid` |
| F7 | AUTH tới nhưng HELLO chưa xong / sai thứ tự, hoặc kind sau-confirm tới trước SESSION_CONFIRM | Bên nhận | Đóng ngay | `handshake_error` code `handshake_failed` | `handshake_out_of_order` |
| F8 | SESSION_CONFIRM trust lệch registry | Bên nhận | Đóng ngay | (đóng lặng trong kênh mã hóa) | `trust_mismatch` |
| F9 | Message sau handshake sai seq | Bên nhận | Đóng session | (đóng lặng) | `sequence_gap_or_replay` |
| F10 | AEAD open fail (sai khóa/sửa ciphertext/sai AAD) | Bên nhận | Đóng session ngay, không retry message đó | (đóng lặng) | `aead_open_failed` |
| F11 | Kind lạ / trường lạ trong header | Bên nhận | Đóng session | (đóng lặng) | `unknown_message_kind` |
| F12 | Frame vượt giới hạn kích thước | Bên nhận | Đóng session, không đọc tiếp | (đóng lặng) | `oversize_frame` |
| F13 | Rekey thất bại (timeout / ECDH sai) | Cả hai | Đóng session, bắt tay lại từ đầu nếu cần | (đóng lặng) | `rekey_failed` |
| F14 | Peer đóng TCP giữa handshake | Bên còn lại | Dọn session, zeroize khóa | — | `handshake_aborted` (chỉ khi đã qua HELLO; đóng trước HELLO thì không ghi — tránh spam audit do quét cổng) |

Quy tắc chung: lỗi handshake TRƯỚC khi AUTH verify xong thì phản hồi (nếu có) là lỗi chung không phân biệt nguyên nhân (trừ code `version_mismatch` — version không phải thông tin nhạy cảm); lỗi SAU khi kênh mã hóa mở thì đóng lặng (kẻ nghe lén không học được gì thêm từ lỗi trong kênh mã hóa, và peer thật sẽ thấy session đứt + tự bắt tay lại). Không bao giờ gửi lại cùng một message sau lỗi AEAD/seq — session chết là chết, bắt tay lại là đường duy nhất.

---

## 3. Pairing ceremony + danh tính thiết bị (P7.0 mục 3, P7.1)

### 3.1 Quyết định: phân biệt hai việc mà `docs/pairing.md` gộp chung — ĐÃ CHỐT (D-DESIGN7-1)

`docs/pairing.md` cấm pairing qua mạng với nguyên văn: "pair request on the wire must never be added" — endpoint không bao giờ được là "token dispenser", controller không được xin secret từ endpoint qua mạng. Bản này GIỮ nguyên văn cấm đó cho đúng việc đó, và phân biệt rõ (phân biệt này đã chốt, không còn mở):

- **VẪN CẤM:** bất kỳ request nào mà phía trả lời phải GỬI secret (token, private key, fingerprint để auto-accept) qua mạng. Không có kind nào trong §2.5 trả về secret. Không có "show me the fingerprint" auto-accept.
- **CHO PHÉP (chỉ cho friends):** hai thiết bị trao đổi public key + transcript + chữ ký qua mạng, trong đó secret duy nhất (SAS 6 số) đi NGOÀI băng (mắt user + so sánh/gõ tay), và fingerprint được user SO SÁNH bằng mắt, không auto-accept. Kẻ nghe lén mạng thấy được public key và chữ ký, nhưng không có SAS thì không hoàn tất pairing (§3.3). SAS ngoài băng + handshake xác thực lẫn nhau KHÔNG đồng nghĩa với "xin secret qua mạng": không khóa hay thông tin bí mật nào đi qua mạng trong ceremony.
- Phương án pairing hoàn toàn ngoài băng (user gõ tay cả public key/fingerprint 64 hex) đã được cân nhắc và LOẠI vì không thực tế và dễ sai; nó không phải phương án dự phòng của bản này. Phân biệt trên là quyết định cuối.

### 3.2 Danh tính thiết bị: keypair riêng, lưu trong kho khóa thiết bị (MỚI)

- **Mỗi body có đúng một khóa danh tính dài hạn:** ECDSA P-256 keypair, sinh bằng `ring::rand::SystemRandom` + API ký của ring, lần đầu tiên body cần pairing (lazy: body chưa bao giờ pair thì chưa có khóa; không sinh khóa lúc cài đặt để tránh tạo danh tính không ai quản lý). Khóa này CHỈ dùng để ký (transcript §2.4, gói handoff §7.3) — không bao giờ dùng cho ECDH (ECDH dùng khóa ephemeral mỗi session). Một khóa, một mục đích.
- **`device_id`:** 32 ký tự hex = 16 byte đầu của SHA-256(public key dạng uncompressed 65 byte). Fingerprint đầy đủ = SHA-256(public key) 64 hex, hiển thị theo nhóm 4 ký tự cho user so sánh. `device_id` là khóa chính trong registry; fingerprint là thứ user so sánh.
- **Lưu trữ (MỚI — `DeviceKeyStore`):** private key (PKCS#8 DER) lưu trong 1 file duy nhất trong data dir (`device-identity.key`), mode `0600` từ lần ghi đầu, đọc qua descriptor đã kiểm tra (`O_NOFOLLOW`, regular file, quyền không mở hơn owner-only) — đúng kỷ luật `open_checked_file` của `FileVault` (`vault.rs`), tổng quát hóa sang đọc byte (FileVault chỉ đọc text). Ghi file: tạo mới thì fail nếu đã tồn tại (không ghi đè khóa đang sống qua API thường — xoay key đi đường §3.6 có xác nhận riêng); đọc trả về `Zeroizing<Vec<u8>>`; không bao giờ log, không bao giờ đưa vào sync/backup (xem §4.2), không bao giờ rời khỏi máy. File này KHÔNG qua `Vault::get` (API đó trả String, chỉ đọc) — module mới, mượn kỷ luật, không ép API cũ.
- **Vì sao không dùng `KeychainVault`/OS keyring trong v1:** `KeychainVault` chỉ đọc, không có API ghi (xem §0.4); Windows Credential Manager / Linux Secret Service chưa có trong cây chút nào. Ép khóa thiết bị vào keyring trong Phase 7 đồng nghĩa viết 3 backend ghi keyring chưa kiểm chứng — ngoài phạm vi, rủi ro cao. File 0600 + kỷ luật descriptor-check là mức bảo vệ đã có tiền lệ trong cây và đủ trong threat model (T10 ngoài phạm vi). Keyring là cải tiến tương lai, ghi ở §13.3 (D-DESIGN7-3).
- **Khóa public + fingerprint KHÔNG phải secret:** được hiển thị, được gửi trong HELLO, được lưu trong registry của peer. Private key là secret duy nhất của danh tính.
- **Mất khóa = mất danh tính.** Không có khôi phục khóa từ backup (khóa không vào backup — đúng như tiền lệ "identity key không vào backup" trong GAP-CHECK §1.3). Mất file khóa (xóa data dir, hỏng đĩa) thì body phải pair lại từ đầu với danh tính mới; peer cũ thấy device_id mới và phải pair lại (không có đường "đây vẫn là tôi, tin đi"). UI và docs phải hiện đúng văn bản cảnh báo sau, tại màn hình quản lý thiết bị (§12.3) và khi user xóa data dir: "Nếu mất file khóa này, thiết bị này sẽ mất danh tính bảo mật và phải ghép đôi lại từ đầu với từng thiết bị đã pair. Khóa không được sao lưu, không được đồng bộ, và không thể khôi phục. Đừng xóa thư mục dữ liệu nếu bạn chưa hiểu hậu quả này."

### 3.3 Ceremony: SAS 6 số + fingerprint (quyết định chi tiết)

Actors: hai body A (bên khởi tạo ceremony) và B (bên nghe), cùng một user đứng giữa (hoặc user + người được tin cậy cầm thiết bị guest). Cả hai body đều đang chạy, đều ở màn hình pairing. Ceremony dùng kênh TCP trực tiếp tới địa chỉ B do user nhập tay (IP:cổng, §4.7) — không discovery, không QR, không broadcast (kế thừa `pairing.md`). Lưu ý tên gọi: chữ A/B trong §3.3 này chỉ là tên hai phía của ceremony (A = bên khởi tạo, B = bên nghe), KHÔNG phải quy ước A/B theo thứ tự device_id của §2.2/§2.4; hai cách gọi tên độc lập, không được lẫn. Trường nonce trong message ceremony đều tên là `nonce` (mỗi bên sinh một nonce 16 byte riêng).

**Hai luồng SAS — luồng chính là luồng so sánh.** Luồng chính (mặc định, dùng khi cả hai bên đều có màn hình dùng được): CẢ HAI bên cùng hiển thị SAS, user so sánh hai SAS bằng mắt và xác nhận riêng từng phía. Luồng fallback (dùng khi một bên không có màn hình dùng được, gồm ESP32): A hiển thị SAS, user gõ 6 số đó vào B. Cả hai luồng đều bắt buộc kiểm tra fingerprint (§3.3, mục UX fingerprint dưới đây) và đều dùng chung các bước 1–2 và 5–7; chỉ bước 3–4 khác nhau ở cách SAS được đối chiếu. Thiết bị không màn hình dùng được luồng fallback; thiết bị có màn hình nhưng user chọn gõ tay vẫn được, nhưng UI mặc định mở luồng so sánh.

Thứ tự (tên message mới, chỉ dùng trong ceremony):

```text
1. A → B (TCP, plaintext, kind "pair_request"):
   { v:2, kind:"pair_request", device_id:A, identity_pub:A_pub,
     ecdh_pub:A_eph_pub, nonce:16B random hex, wanted_trust:"owner"|"guest" }
   B chỉ chấp nhận pair_request khi B đang ở CHẾ ĐỘ PAIRING do user bật
   rõ ràng trên B (nút "Cho phép pair" + đếm ngược hiển thị). Ngoài chế
   độ đó, B từ chối mọi pair_request, không trả gì ngoài lỗi chung.
2. B → A (kind "pair_response"):
   { v:2, kind:"pair_response", device_id:B, identity_pub:B_pub,
     ecdh_pub:B_eph_pub, nonce:16B random hex }
3. Cả hai tính transcript_hash như §2.4 (transcript = nối có độ dài của
   version + byte JSON của pair_request + byte JSON của pair_response,
   theo thứ tự cố định: request trước, response sau) và suy ra SAS:
   SAS = HMAC-SHA256(key = transcript_hash, msg = "mimint friends v2 sas")
         → lấy 4 byte đầu → mod 1.000.000 → 6 chữ số, đệm 0.
   LUỒNG SO SÁNH (mặc định): A và B CÙNG hiển thị SAS 6 chữ số lớn +
   fingerprint của peer (dạng rút gọn có mở rộng, xem dưới) + fingerprint
   của chính mình để đối chiếu chéo.
   LUỒNG GÕ TAY (fallback): A HIỂN THỊ SAS; B hiển thị ô gõ 6 số +
   fingerprint của A. B không hiển thị SAS của nó.
4. QUYẾT ĐỊNH TRUST PHÍA B + XÁC NHẬN PHÍA B (độc lập):
   - Luồng so sánh: user so sánh SAS trên hai màn hình. Trên B, user chọn
     mức trust B CẤP CHO A (Owner/Guest, hiển thị rõ quyền từng mức), đối
     chiếu fingerprint của A, rồi bấm "SAS khớp — xác nhận" HOẶC "SAS không
     khớp — hủy". Bấm "không khớp" = ceremony thất bại (xem §3.4).
   - Luồng gõ tay: user đọc SAS trên A, gõ vào B, chọn mức trust B cấp cho
     A, đối chiếu fingerprint của A, rồi bấm Xác nhận trên B.
   Quyết định trust này là của user TRÊN B, chỉ quyết định mức B cấp cho A.
5. B kiểm tra: luồng gõ tay — SAS user gõ có bằng SAS B tự tính không;
   luồng so sánh — user đã bấm "khớp" trên B (máy không tự biết SAS có khớp
   thật không; xem §3.4 về giới hạn này).
   - Đạt → B gửi kind "pair_confirm" { sig_B = ECDSA_sign(B_priv,
     transcript_hash), granted_trust = mức B cấp cho A }.
   - Không đạt → B gửi lỗi chung kind "pairing_error" (không nói sai ở đâu),
     đếm 1 lần sai (§3.4), đóng kết nối. Không gửi chữ ký khi SAS sai.
6. A nhận pair_confirm, verify chữ ký B bằng public key vừa nhận (chưa pin),
   rồi A HIỂN THỊ fingerprint của B lần cuối + mức trust B đã cấp cho A.
   QUYẾT ĐỊNH TRUST PHÍA A + XÁC NHẬN PHÍA A (độc lập): user trên A chọn mức
   trust A CẤP CHO B (mặc định hiển thị = mức B đã cấp cho A, user A được
   chọn lại độc lập — hai phía không ràng buộc chéo), đối chiếu fingerprint
   của B, rồi bấm Xác nhận trên A. User bấm Xác nhận trên A → A gửi kind
   "pair_accept" { sig_A, granted_trust = mức A cấp cho B } và LƯU pin của B
   vào registry (device_id, pubkey, fingerprint, trust, thời điểm). Đây là
   bước A lưu pin — bước 6, không phải bước 7.
7. B nhận pair_accept, verify chữ ký A, LƯU pin của A vào registry. Đây là
   bước B lưu pin — bước 7. Cả hai ghi audit `pairing_completed` (device_id
   + fingerprint + trust, KHÔNG ghi SAS). Ceremony kết thúc. Hai bên bắt tay
   session thường (§2.4) để chứng minh pairing hoạt động — pairing chỉ được
   coi là xong khi 1 session thường bắt tay thành công + 1 message mã hóa qua
   lại được.
```

Quy tắc bổ sung:

- **Hai quyết định trust độc lập.** Trust là thuộc tính của registry local: mỗi bên lưu mức trust mà user TRÊN MÁY ĐÓ đã cấp cho peer. Hai quyết định không ràng buộc chéo; mỗi phía tự chịu trách nhiệm về mức trust nó cấp. UI dùng nhãn theo hướng, không dùng nhãn chung: "Thiết bị này cấp cho <tên peer>: Owner/Guest" và "<tên peer> cấp cho thiết bị này: Owner/Guest", kèm danh sách quyền được cấp và quyền không được cấp. CẤM dùng nhãn chung "Đã kết nối" hay "Owner pairing" để đại diện cho cả hai hướng. Khi một phía chưa lưu pin xong, trạng thái hiển thị là "Pairing chưa hoàn tất", không phải "Đã kết nối".
- **UX fingerprint (bắt buộc cho cả hai luồng).** Màn hình chính hiển thị fingerprint RÚT GỌN: 12 ký tự đầu + "…" + 12 ký tự cuối, nhóm theo 4 ký tự, kèm thao tác mở rộng xem đầy đủ 64 ký tự và nút sao chép fingerprint. Fingerprint đầy đủ nằm trong phần Chi tiết bảo mật. Mỗi phía có trạng thái xác nhận riêng: UI phân biệt "fingerprint đã hiển thị" với "user đã đối chiếu-xác nhận" — user phải bấm riêng "Tôi đã đối chiếu fingerprint" trên từng phía; ceremony không hoàn tất nếu thiếu bước này, trừ khi user bấm "Bỏ qua kiểm tra fingerprint", đọc cảnh báo rủi ro hiện ra, xác nhận đã hiểu, và audit ghi `fingerprint_check_skipped`. Không cho bỏ qua fingerprint mà không có cảnh báo.
- **Hết hạn:** ceremony session hết hạn sau **5 phút** kể từ `pair_request` (đúng trần CRITERIA ≤ 5 phút). Hết hạn → đóng, SAS vô giá trị, phải bắt đầu lại từ đầu (nonce + ECDH mới). SAS dùng 1 lần: SAS của ceremony này không dùng được cho ceremony khác (transcript khác → SAS khác).
- **Chế độ pairing trên B:** phải do user bật tường minh, tự tắt sau 5 phút hoặc sau 1 ceremony (thành công hay thất bại đều tắt — muốn pair tiếp thì bật lại). Khi chế độ pairing đang bật, UI hiển thị rõ "đang cho phép pair" + đếm ngược. Không có chế độ pairing luôn-bật.
- **MITM trong ceremony:** kẻ đứng giữa sửa public key trong pair_request/response → transcript hai bên khác nhau → SAS hai bên khác nhau → luồng gõ tay: user gõ SAS của A vào B sẽ sai (B tính SAS khác) → thất bại + audit; luồng so sánh: user thấy hai SAS lệch và bấm "không khớp" → thất bại + audit. Kẻ đứng giữa không đọc được màn hình hai thiết bị nên không biết SAS đúng. Đây là toàn bộ ý nghĩa của SAS — và là lý do SAS phải đi ngoài băng, không bao giờ gửi qua mạng trong ceremony (B không bao giờ gửi SAS của nó; A không bao giờ gửi SAS qua mạng; chỉ user so sánh/gõ).
- **Fingerprint là lớp thứ hai:** SAS 6 số (~20 bit) chống MITM ngẫu nhiên, nhưng kẻ tấn công đoán SAS có ~1/1.000.000 cơ hội mỗi lần thử + lockout (§3.4) chặn thử lặp. Fingerprint 256 bit là lớp xác minh mạnh: user so sánh fingerprint thì MITM gần như không qua được ngay cả khi đoán trúng SAS. UI phải trình bày CẢ HAI, không chỉ SAS.
- **Không QR trong v1.** Kế thừa `pairing.md` (mắt + bàn phím). QR mang fingerprint là cải tiến tương lai (§13.3) — QR không làm ceremony an toàn hơn, chỉ nhanh hơn; và QR trên màn hình bị chụp từ xa là bề mặt mới. Không làm ở Phase 7.
- **`wanted_trust`:** A xin mức trust nào (owner/guest). Mức này chỉ là đề nghị hiển thị trên B; mức cuối cùng B cấp cho A do user trên B chọn ở bước 4, và mức A cấp cho B do user trên A chọn ở bước 6. A không tự cấp owner cho mình trên B.

### 3.4 Sai mã, hết hạn, khóa tạm thời + UX ceremony

- **Sai SAS (luồng gõ tay):** từ chối + audit `pairing_failed` (ghi device_id xin pair + lý do chung `sas_mismatch`, không ghi SAS đã gõ). Đếm số lần sai LIÊN TIẾP cho mỗi device_id xin pair (lưu trong registry store, persistent qua khởi động lại — lockout không bị reset bằng cách restart app).
- **SAS lệch (luồng so sánh):** user bấm "SAS không khớp — hủy" trên một phía = ceremony thất bại + audit `pairing_failed` (lý do `sas_mismatch_reported`) + đếm 1 lần sai và lockout áp dụng Y NHƯ sai mã ở luồng gõ tay. Giới hạn nói thật: nếu user bấm "SAS khớp — xác nhận" dù hai SAS thực tế lệch, máy không phát hiện được — rủi ro này thuộc T12 (lỗi của chính user), giảm thiểu bằng fingerprint bắt buộc + xác nhận riêng từng phía, không tuyên bố máy đã kiểm chứng được.
- **N (quyết định): N = 5.** Sau 5 lần sai SAS liên tiếp từ cùng một device_id (hoặc từ cùng một địa chỉ IP nếu device_id thay đổi mỗi lần — chống đổi id để reset đếm), body bị xin pair KHÓA ceremony pairing trong **15 phút** đối với nguồn đó: mọi `pair_request` từ nguồn đó bị từ chối ngay, không hiển thị SAS, audit ghi `pairing_locked`. Pairing thành công reset đếm về 0. Hết 15 phút, đếm reset. Lý do chọn 5/15 phút: đủ cho user gõ nhầm vài lần, đủ chặn đoán mò (5 lần thử mỗi 15 phút → ~480 lần/ngày → xác suất đoán trúng SAS 6 số trong 1 ngày < 0,05%, và mỗi lần thử đều audit + hiển thị trên màn hình B). Phương án đã loại: khóa vĩnh viễn tới khi user mở (quá nặng, user tự khóa mình); khóa 1 phút (quá nhẹ, đoán mò khả thi).
- **Hết hạn:** ceremony quá 5 phút → đóng + audit `pairing_expired`. Không tính vào đếm sai (hết hạn không phải tấn công).
- **Rate limit chung:** tối đa 1 ceremony đang chạy tại 1 thời điểm trên mỗi body; `pair_request` thứ hai trong lúc ceremony đang chạy bị từ chối. Tối đa 10 `pair_request` mỗi phút từ mọi nguồn (chống spam làm phiền UI); vượt thì từ chối lặng + audit tổng hợp (không spam audit từng request).
- **UX ceremony (bắt buộc):** màn hình pairing hiển thị đồng hồ đếm ngược 5 phút; thông báo rõ khi ceremony hết hạn; nút "Hủy ceremony" dừng ngay và đóng kết nối; hướng dẫn bắt đầu lại sau khi hủy/hết hạn/thất bại ("Bấm 'Cho phép pair' lại trên thiết bị kia, rồi bắt đầu lại từ đầu"); khi bị lockout, UI nêu lý do lockout + thời điểm được thử lại ("Pairing từ thiết bị này đang bị khóa tới HH:MM do 5 lần sai liên tiếp."). Lockout CHỈ áp dụng cho pairing từ nguồn bị khóa — thiết bị không bị khóa vẫn pair bình thường, và lockout không khóa bất kỳ chức năng nào khác của thiết bị.
- **Thông báo user thấy cho từng ca:** hết hạn — "Pairing đã hết hạn sau 5 phút. SAS cũ không còn giá trị. Hãy bắt đầu lại từ đầu."; sai mã (gõ tay) — "Mã SAS không đúng. Kiểm tra lại 6 số trên màn hình thiết bị kia và thử lại. Còn X lần thử trước khi bị khóa tạm thời."; SAS lệch (so sánh) — "Hai mã SAS không khớp. Ceremony đã dừng. Hãy bắt đầu lại từ đầu và kiểm tra fingerprint."; lockout — như câu lockout ở trên; không ở chế độ pairing — "Thiết bị kia chưa bật chế độ pairing. Hãy bật 'Cho phép pair' trên thiết bị đó rồi thử lại." Không hiển thị lỗi chung chung mà không nói user phải làm gì tiếp theo.

### 3.5 Hai mức trust: owner/guest

| Quyền | Owner | Guest |
|---|---|---|
| Bắt tay session + trao đổi message mã hóa | Có | Có |
| Sync dữ liệu (§4) | Có (theo selective sync) | KHÔNG — guest không nhận/gửi record sync nào |
| Phê duyệt xuyên thiết bị (§6) | Có (trả lời thẻ đang chờ) | KHÔNG |
| Handoff (§7) | Có (gửi + nhận) | KHÔNG nhận; không gửi |
| Lái body qua hub (§5) | Có (theo allow-list owner) | Chỉ method đọc (§5.3), như guest Android |
| Pair tiếp thiết bị khác (giới thiệu peer) | Không áp dụng (không có giới thiệu peer trong v1) | KHÔNG (không có cơ chế nào cho phép điều này) |
| Thấy dữ liệu private của owner | Có (chính là owner) | KHÔNG — guest chỉ thấy màn hình qua method đọc + dữ liệu trong message nó được gửi; không thấy notebook private, SOUL, skill, profile |
| Thu hồi/xoay key của peer khác | Không (mỗi bên chỉ quản lý registry của mình) | KHÔNG |

Guest sandbox: guest là "GuestHost" của friends — allow-list hẹp, deny-by-default, enforce phía server (bên bị lái), không tin claim của guest. Mọi method ngoài allow-list → `forbidden_role` + audit, đúng tiền lệ Android `dispatch` (`android.rs:1461-1469`).

### 3.6 Thu hồi + xoay key (phân biệt thu hồi và xóa)

- **Thu hồi 1 thiết bị (nhãn UI: "Thu hồi"):** user bấm Thu hồi trên 1 peer trong registry → entry đó bị đánh dấu `revoked` (giữ lại bản ghi để audit + để từ chối rõ ràng, không xóa hẳn) → mọi kết nối/sync sau đó từ thiết bị đó bị từ chối ở bước kiểm tra pin (§2.4 bước 1); session đang mở với thiết bị đó bị đóng ngay (hub/body kiểm tra registry khi có sự kiện thu hồi — thu hồi là sự kiện local, áp dụng ngay trong tiến trình). Thu hồi KHÔNG cần peer đồng ý, KHÔNG cần mạng. Thu hồi là một chiều: thu hồi trên A không tự thu hồi trên B (B vẫn tin A cho tới khi user thu hồi trên B) — UI phải nói rõ "thu hồi chỉ có hiệu lực trên máy này". Giải thích hiển thị kèm nút Thu hồi: "Thu hồi: thiết bị vẫn còn trong danh sách nhưng không được phép kết nối tới thiết bị này nữa."
- **Xoay key của chính mình (nhãn UI: "Đặt lại danh tính bảo mật của thiết bị này"):** user bấm → sinh keypair danh tính mới → device_id mới → TẤT CẢ pairing cũ của body này trở nên vô giá trị (peer cũ pin public key cũ, device_id cũ) → user phải pair lại từng peer. Đây là hành động có xác nhận riêng với cảnh báo đầy đủ phạm vi ảnh hưởng: "Danh tính bảo mật của thiết bị này sẽ thay đổi. Tất cả peer đã pair sẽ mất hiệu lực. Bạn phải ghép đôi lại từng thiết bị. Không có giai đoạn tương thích song song — key cũ bị từ chối ngay." + audit. Key cũ bị từ chối ngay sau khi xoay (không có grace period, không có "key cũ vẫn dùng được 1 thời gian") — đúng tiền lệ `docs/pairing.md` ("no grace period").
- **`revoke_notice` (best-effort):** khi body xoay key, nó GỬI THỬ `revoke_notice` tới các peer cũ (nếu nối được) để peer hiển thị "thiết bị X đã xoay key, cần pair lại". Peer KHÔNG tin notice này mù: notice không tự xóa pin, không tự pair lại; nó chỉ là gợi ý hiển thị. Kẻ giả mạo gửi notice giả cũng chỉ gây hiển thị sai, không gây mất pairing thật. (Nếu không nối được peer thì không sao — peer sẽ tự thấy handshake thất bại khi cần.)
- **Xóa entry khỏi registry (nhãn UI: "Xóa khỏi danh sách"):** sau thu hồi, user có thể Xóa hẳn entry (xóa bản ghi + audit ghi `registry_entry_deleted`). Xóa hẳn khác thu hồi: entry đã xóa thì peer đó trở lại trạng thái "lạ" (muốn pair lại thì ceremony mới từ đầu, như chưa từng pair). Giải thích hiển thị kèm nút Xóa: "Xóa: xóa thông tin thiết bị khỏi danh sách; nếu thiết bị đó kết nối lại sẽ được xem là thiết bị lạ." Hai hành động Thu hồi và Xóa KHÔNG đặt cạnh nhau với nhãn giống nhau: Thu hồi nằm trong hàng thiết bị (hành động chính), Xóa nằm trong menu chi tiết của entry đã thu hồi (chỉ hiện sau khi đã thu hồi), mỗi nút kèm giải thích riêng như trên để user không nhầm.

### 3.7 Device registry: lưu local, có UI quản lý (MỚI)

- **Lưu trữ:** SQLite file riêng `peers.db` trong data dir (cùng thư mục với notebook DB, file khác — tránh trộn migration notebook với migration peer). Bảng `devices`: `device_id` (TEXT PK), `name` (TEXT, tên do user đặt lúc pair, sửa được), `identity_pub_hex` (TEXT, 130 hex), `fingerprint_hex` (TEXT, 64 hex), `trust` (TEXT `owner`/`guest`), `paired_at_ms` (INTEGER), `last_seen_ms` (INTEGER, cập nhật mỗi session thành công), `key_generation` (INTEGER, tăng khi peer xoay key + pair lại — để phân biệt "pair lại" với "pair mới"), `revoked` (INTEGER 0/1), `revoked_at_ms` (INTEGER NULL). Bảng `pairing_attempts`: `source` (device_id hoặc IP), `fail_count`, `locked_until_ms`. Bảng `sync_state`: xem §4.7. Không lưu private key, không lưu SAS, không lưu khóa phiên trong DB này.
- **Tên thiết bị:** user đặt lúc pair (mặc định: tên OS/hostname đã sanitize, cắt 40 ký tự, chỉ hiển thị, không dùng làm khóa — như nhãn agent Phase 6). Hai peer trùng tên vẫn phân biệt bằng device_id; UI hiển thị tên + 8 ký tự đầu fingerprint để phân biệt.
- **UI quản lý (yêu cầu):** danh sách thiết bị đã pair (tên, fingerprint rút gọn, trust theo hướng — "thiết bị này cấp cho peer" và "peer cấp cho thiết bị này", thời điểm pair, lần thấy cuối, trạng thái revoked, trạng thái "Pairing chưa hoàn tất" nếu có); nút Thu hồi / Xóa / Đổi tên / Đổi trust (hạ Owner→Guest thì hiệu lực ngay; nâng Guest→Owner YÊU CẦU ceremony pair lại — không nâng bằng 1 lần bấm, vì nâng trust là cấp quyền mới; mỗi hành động kèm giải thích theo §3.6); nút "Đặt lại danh tính bảo mật của thiết bị này" (có xác nhận + cảnh báo §3.6); nút Bật chế độ pairing (đếm ngược 5 phút); trạng thái khóa pairing (nếu đang bị lockout, nêu nguồn bị khóa + thời điểm thử lại). CLI in các thông tin tương đương dưới dạng text, dùng cùng từ ngữ với GUI (xem bảng nhất quán §12.4).
- **Registry không sync.** Registry là local-only tuyệt đối: không có record kind nào mang registry, không có "giới thiệu peer" (A giới thiệu B cho C). Mỗi pairing là ceremony trực tiếp giữa hai body. Lý do: giới thiệu peer là bề mặt trust-transitivity khó kiểm chứng; v1 không cần (≤ 4 body, pair tay từng cặp là đủ).

### 3.8 Kiểm chứng (pairing)

- `pairing_success_two_instances`: 2 instance thật trên vega, ceremony đầy đủ theo luồng so sánh mặc định (SAS hiển thị hai bên, đối chiếu fingerprint, xác nhận riêng từng phía, hai quyết định trust độc lập) → registry cả hai bên có entry đúng (kèm trust theo hướng), session thường bắt tay được.
- `pairing_fallback_typing_flow_success`: ceremony đầy đủ theo luồng gõ tay (A hiển thị SAS, gõ vào B) → pair thành công; fingerprint vẫn bắt buộc đối chiếu như luồng chính.
- `pairing_wrong_sas_rejected_and_audited`: luồng gõ tay, gõ sai SAS → từ chối, audit `pairing_failed`, không entry nào được lưu, không chữ ký nào được gửi.
- `pairing_compare_flow_mismatch_reported_fails_and_counts`: luồng so sánh, user báo "SAS không khớp" → ceremony thất bại, audit `pairing_failed`, đếm 1 lần sai như sai mã; 5 lần liên tiếp → lockout như §3.4.
- `pairing_expired`: ceremony để quá 5 phút (đồng hồ test) → hết hạn, SAS cũ không dùng được cho ceremony mới.
- `pairing_lockout_after_5_failures`: 5 lần sai liên tiếp → lần thứ 6 bị từ chối ngay cả với SAS đúng; sau 15 phút (đồng hồ test) lại pair được; restart app giữa chừng không reset lockout; lockout chỉ chặn nguồn bị khóa, nguồn khác vẫn pair được.
- `pairing_mitm_fails`: luồng gõ tay, harness sửa public key trong pair_request → SAS hai bên lệch → gõ SAS của A vào B sai → ceremony thất bại.
- `pairing_compare_flow_mitm_fails`: luồng so sánh, harness sửa public key trong pair_request → hai SAS hiển thị lệch → user (harness mô phỏng) báo không khớp → ceremony thất bại, không pin nào được lưu.
- `pairing_fingerprint_skip_requires_warning_and_audits`: bỏ qua kiểm tra fingerprint mà không qua cảnh báo → ceremony không hoàn tất; bỏ qua sau khi xác nhận cảnh báo → hoàn tất nhưng audit ghi `fingerprint_check_skipped`.
- `pairing_trust_both_directions_independent`: B cấp Guest cho A, A cấp Owner cho B → registry mỗi bên lưu đúng mức mình cấp; UI mỗi bên hiển thị đúng nhãn theo hướng; trạng thái là "Pairing chưa hoàn tất" cho tới khi cả hai bên đều lưu pin xong.
- `pairing_requires_pairing_mode`: B không bật chế độ pairing → `pair_request` bị từ chối, không hiển thị gì.
- `pairing_revoke_cuts_access`: pair xong → thu hồi trên B → A nối lại bị từ chối; session đang mở bị đóng.
- `pairing_rotate_invalidates_old_key`: B xoay key → A nối bằng pin cũ bị từ chối; pair lại với danh tính mới thì được; `key_generation` tăng.
- `pairing_guest_cannot_sync_or_approve`: pair guest → sync_offer từ guest bị từ chối, approval từ guest bị từ chối, guest không thấy record nào.
- `pairing_trust_not_self_granted`: A xin owner nhưng user trên B chọn guest → registry B lưu guest; A không có quyền owner trên B.
- `pairing_pin_stored_at_correct_steps`: A lưu pin của B ngay khi gửi `pair_accept` (bước 6), B lưu pin của A khi nhận `pair_accept` (bước 7); nếu kết nối đứt giữa bước 6 và 7, A có pin còn B chưa có → trạng thái "Pairing chưa hoàn tất", pair lại từ đầu thì hội tụ, không có trạng thái nửa-vời được coi là xong.
- `device_key_file_permissions`: file khóa thiết bị có mode 0600; file symlink/thế giới đọc được → từ chối đọc (fail-closed); file chứa khóa không bao giờ xuất hiện trong payload sync (kiểm tra cùng canary test §4.8).

---

## 4. Đồng bộ E2E (P7.0 mục 4, P7.2 + P1 selective sync)

### 4.1 Quyết định tổng thể

- Sync là **sao chép record có chọn lọc giữa hai body đã pair (owner)**, qua kênh E2E §2, theo phiên sync do user khởi xướng hoặc theo lịch nhẹ (xem §4.7). Không có sync nền liên tục âm thầm: mỗi phiên sync đều có điểm bắt đầu/kết thúc nhìn thấy được trong UI + audit (số record gửi/nhận/conflict). Lý do: user phải luôn biết dữ liệu nào vừa di chuyển đi đâu; sync âm thầm là bề mặt rò rỉ không kiểm chứng được.
- **Local-first tuyệt đối:** mất peer, mất mạng, sync thất bại giữa chừng → body vẫn chạy bình thường với dữ liệu local. Sync không bao giờ xóa dữ liệu local trừ khi user xác nhận xóa lan truyền (§4.5).
- **Không merge âm thầm, không last-write-wins âm thầm** (D6). Mọi xung đột đều hiện ra cho user chọn (§4.4).

### 4.2 Tập dữ liệu được sync — và tập CẤM (allow-list ép bằng code)

| Được sync (record kind) | Nguồn trong cây | Điều kiện |
|---|---|---|
| `soul_file` | `SoulStore` (§0.4) | Mọi file SOUL trong data dir (theo tên file hợp lệ). Body của SOUL + version marker. |
| `skill` | `SkillStore`, chỉ skill đã promote | Skill draft, skill chưa qua cổng, skill đã retire: KHÔNG sync. |
| `notebook_note` | `Notebook`, chỉ dòng `shared=true` | Dòng private (`shared=false`) KHÔNG BAO GIỜ sync, kể cả khi user bật sync notebook. |
| `todo` / `plan` | Store MỚI §4.6 | Record tối thiểu do Phase 7 định nghĩa. |
| `naming_profile` | `ProfileEntry` (§0.4) | Hồ sơ đặt tên D13b: sync hồ sơ liên body (việc Phase 5 để lại). Backend/key KHÔNG đi kèm (xem dưới). |

| KHÔNG BAO GIỜ sync (deny tuyệt đối) | Cách ép |
|---|---|
| Vault/secret/key (API key, khóa thiết bị, khóa phiên) | Serializer sync chỉ biết 5 kind trên; không có đường code nào đọc vault/key store đưa vào record. Test canary §4.8. |
| History chat (messages, transcript lượt) | Không có record kind cho history; brain messages không bao giờ vào serializer. |
| Audit log | Không có record kind; audit là local-only, hash-chain tương lai (Phase 8) cũng local. |
| Trajectory quarantine / experience store | Không có record kind. Học ké từ bạn bè bị cấm (PROPOSALS §5) — experience không sync. |
| Notebook dòng private | Serializer lọc `shared=true` ở tầng đọc DB (dùng `search_for`/`all_notes` + lọc, không tin flag do caller truyền). |
| Registry thiết bị, sync state, lease, config file nguyên văn | Không có record kind. Config sync duy nhất là `naming_profile` (đã validate, không chứa secret). |
| SOUL history, skill validation evidence thô, file đính kèm nhị phân | Không có record kind. |

**Ép bằng code, không bằng lời hứa:** module sync có đúng một hàm "đọc record để gửi" cho mỗi kind, và đúng một hàm "ghi record đã nhận" cho mỗi kind. Không có đường generic "sync file bất kỳ" hay "sync bảng bất kỳ". Test cổng quét: grep toàn bộ code sync không có lời gọi tới `Vault::get`, `DeviceKeyStore` đọc private, `AuditLog` đọc file, hay đường đọc history. Test canary (§4.8) đặt secret giả vào vault + notebook private + history, chạy sync đầy đủ, đọc record sync thô và khẳng định secret không xuất hiện.

**Hồ sơ đặt tên và backend:** record `naming_profile` mang đúng các trường của `ProfileEntry` (tên, SOUL file, chain backend THEO TÊN, voice label, skills theo tên, proactive, budget, privacy, cost cap, avatar). Nó KHÔNG mang: API key, key file path, backend base_url, hay bất kỳ secret nào. Máy nhận chạy `ProfileSet::resolve` như đọc config local: backend tên lạ → profile bị drop + warning (luật hiện có), không tự tạo backend, không tự điền key. User muốn profile chạy được trên máy nhận thì tự cấu hình backend cùng tên trên máy đó — sync không làm hộ việc đó.

### 4.3 Định danh + versioning record

- **`sync_id` ổn định liên body (MỚI):** mỗi record sync có `sync_id` là chuỗi hex 32 ký tự (16 byte ngẫu nhiên từ `SystemRandom`, hex bằng helper tự viết theo mẫu `hex_lower` của `android.rs` — không có dep uuid/base64 trực tiếp trong cây nên tự sinh id; va chạm 128 bit là không đáng kể và test khẳng định tính duy nhất trong DB). `sync_id` sinh 1 lần khi record được tạo (hoặc khi record cũ lần đầu được chạm tới để sync), không bao giờ đổi, kể cả khi record bị sửa/xóa (tombstone giữ nguyên `sync_id`).
  - Notebook: migration `ALTER TABLE notes ADD COLUMN sync_id TEXT` (idempotent, theo mẫu migration Phase 6 ở `notebook.rs:420-434`) + backfill: dòng chưa có `sync_id` được gán khi `add_for` tạo mới (luôn gán) hoặc khi sync lần đầu chạm tới (gán + lưu). Thêm unique index trên `sync_id` (NULL được phép nhiều dòng cho tới khi backfill xong — SQLite unique index cho phép nhiều NULL; sau backfill, mọi dòng shared đều có id). Dòng private cũng được gán id khi tạo (rẻ, đồng nhất) nhưng không sync.
  - SOUL file: `sync_id` = `"soul:" + tên file` (tên file đã là khóa ổn định, validate bởi `is_valid_soul_file_name`). Không cần id ngẫu nhiên.
  - Skill: `sync_id` = `"skill:" + tên skill` (tên là khóa của `SkillStore`).
  - Profile: `sync_id` = `"profile:" + tên profile`.
  - Todo/plan: `sync_id` ngẫu nhiên như notebook.
- **Version:** cặp (Lamport u64, device_id) như §2.3. Mỗi record mang `version_lamport`, `version_device` (device tạo ra version đó), `updated_ms` (wall-clock, chỉ để hiển thị "sửa lúc nào", không dùng để quyết định thắng/thua), `deleted` (tombstone flag), và `origin_device` (device tạo record lần đầu — để hiển thị "tạo ở thiết bị X"). Ngoài ra mỗi record mang **`based_on`**: version (Lamport, device_id) mà version hiện tại được sửa trực tiếp từ đó, hoặc null khi record mới tạo lần đầu. `based_on` là dữ liệu nhân quả duy nhất thuật toán §4.4 dùng để phân biệt successor với concurrent; nó đi kèm trong envelope record (§4.9) và được lưu cùng record ở phía nhận. Record tạo ra khi giải quyết conflict mang thêm **`supersedes_version`**: version của phía bên kia mà bản giải quyết này thay thế (xem §4.4); record thường mang `supersedes_version` null.
- **Nội dung record:** JSON theo kind (schema ở §4.4–§4.6). Kích thước record ≤ 256 KiB (SOUL/skill/note dài hơn thì bị từ chối sync với thông báo rõ — không cắt âm thầm). Hash nội dung: SHA-256 của canonical JSON (dùng `sha256_hex` của `audit.rs:159`) để phát hiện thay đổi mà không cần so sánh toàn bộ body trong sync_offer.

### 4.4 Conflict: thuật toán phát hiện concurrent + giữ cả hai, hiện diff, user chọn

**Thuật toán phát hiện concurrent (bắt buộc, D-DESIGN7-4).** Thuật toán này là luật duy nhất quyết định một record remote được áp dụng, bị bỏ qua, hay tạo conflict. Quy tắc đơn giản "Lamport bằng nhau → conflict, Lamport lớn hơn → cập nhật" BỊ BÁC và không được dùng ở bất kỳ đâu: bằng nhau không phải điều kiện cần hay đủ cho concurrent, và "lớn hơn → cập nhật" là last-write-wins trá hình.

Định nghĩa:

- **Version** của một record là cặp (Lamport, device_id) kèm hash nội dung. Hai version bằng nhau khi cả ba thành phần đều bằng nhau.
- **Sync state `last_seen`:** bảng `sync_state` trong `peers.db`, khóa theo (peer, `sync_id`), giá trị là version mà CẢ HAI bên đã cùng có tại cuối phiên sync thành công gần nhất cho record đó (gọi là version baseline của cặp peer-record đó). `last_seen` chỉ được cập nhật khi: phiên sync kết thúc thành công (§4.7 bước 5), hoặc khi áp dụng một successor theo thủ tục dưới đây. `last_seen` KHÔNG cập nhật khi tạo conflict.
- **"Đã thấy":** một body "đã thấy" version V của record S khi V bằng version local hiện tại của S, hoặc V bằng `last_seen` của (peer, S), hoặc V bằng `based_on` của version local hiện tại. Ngoài ba trường hợp đó, body chưa thấy V.

Thủ tục khi nhận record remote R (version Vr, `based_on` Br, `supersedes_version` Sr, hash Hr) từ peer P cho `sync_id` S, với version local hiện tại Vl (`based_on` Bl) và baseline B = `last_seen`[P][S] (có thể không tồn tại). Xét theo thứ tự, dừng ở quy tắc đầu tiên khớp:

1. **Đã thấy → bỏ qua.** Nếu Vr bằng Vl (cùng version, cùng hash), hoặc Vr bằng B, thì R là version đã biết → bỏ qua, trả ack, không tạo conflict, không ghi đè.
2. **Bản giải quyết conflict → áp dụng.** Nếu Sr bằng Vl (R là bản user tạo ra để giải quyết đúng conflict với version local hiện tại) → áp dụng R, `last_seen` cập nhật thành Vr.
3. **Successor → cập nhật.** Nếu Br bằng Vl (R được sửa trực tiếp từ version local hiện tại, tức phía gửi đã thấy Vl trước khi sửa) → R là successor thật → áp dụng R (ghi đè local), `last_seen` cập nhật thành Vr.
4. **Local là successor → giữ local.** Nếu Bl bằng Vr (local được sửa trực tiếp từ Vr, phía nhận chưa kịp thấy bản local) → giữ local, không tạo conflict; phía gửi sẽ áp dụng bản local khi nhận được nó theo quy tắc 3.
5. **Thiếu lịch sử → conflict bảo thủ.** Nếu không có hàng `last_seen` cho (P, S) mà cả hai bên đều có version khác nhau của S, hoặc R thiếu trường `based_on`, hoặc không quy tắc nào ở trên khớp → tạo conflict. Đặc biệt: sau khi xóa DB (`peers.db` mất, `sync_state` trống) trong khi record vẫn còn, mọi version khác nhau đều thành conflict — không bao giờ ghi đè hay vứt bản nào. Xử lý bảo thủ có thể tạo conflict thừa (false conflict), điều đó chấp nhận được; ghi đè âm thầm thì không.
6. **Không còn trường hợp nào khác.** Mọi cặp version khác nhau mà không bên nào đã thấy version của bên kia đều là concurrent theo đúng nghĩa: concurrent khi và chỉ khi không bên nào đã thấy version của bên kia (theo định nghĩa "đã thấy" ở trên). Không có bước nào trong thủ tục so sánh số Lamport để quyết định.

Record local chưa tồn tại: nếu S chưa có ở local và không có tombstone cho S (§4.5) → nhận R như record mới, `last_seen` đặt thành Vr. Nếu S chưa có ở local nhưng có tombstone → xử lý theo §4.5 (tombstone thắng version cũ hơn hoặc bằng nó; resurrection sau purge theo §4.5/§4.7).

**Ví dụ worked example 1 — successor thật.** Note N, hai thiết bị A và B đã sync xong ở version (Lamport 5, device A), hash h5; baseline B = (5, A) ở cả hai bên. B sửa note → version (6, B), `based_on` (5, A), hash h6. B gửi cho A. A xét: Vr = (6, B) khác Vl = (5, A); Vr khác baseline (5, A); Sr null; Br = (5, A) bằng Vl → quy tắc 3, successor → A áp dụng bản của B, local thành (6, B), baseline cập nhật thành (6, B), Lamport local thành max(5, 6) + 1 = 7 cho lần sửa sau. Không conflict.

**Ví dụ worked example 2 — concurrent thật.** Cùng baseline (5, A). A sửa (không sync giữa chừng) → (6, A), `based_on` (5, A), hash hA. B sửa → (6, B), `based_on` (5, A), hash hB. A nhận bản của B: Vr = (6, B) khác Vl = (6, A); Vr khác baseline; Sr null; Br = (5, A) khác Vl = (6, A) → không phải successor; Bl = (5, A) khác Vr = (6, B) → local cũng không phải successor → quy tắc 5, concurrent → conflict ở cả hai bên, cả hai bản nguyên vẹn, baseline giữ nguyên (5, A). Lưu ý quyết định: hai số Lamport bằng nhau (6 = 6) KHÔNG phải lý do kết luận concurrent. Nếu A đã sửa hai lần (version (7, A), `based_on` (6, A)), bản (6, B) của B vẫn concurrent dù 7 > 6 — so sánh Lamport rồi áp dụng "bản lớn hơn" sẽ làm mất bản sửa của B. Vì vậy không bước nào được so sánh Lamport để quyết định.

**Lưu trữ conflict:** bảng `sync_conflicts` trong `peers.db`: `sync_id`, `kind`, `local_version`, `remote_version`, `local_json`, `remote_json`, `peer_device_id`, `detected_ms`, `resolved` (0/1), `resolution` (NULL/`keep_local`/`keep_remote`/`keep_both`/`merged_by_user`). Conflict chưa resolve thì record local VẪN GIỮ bản local (user tiếp tục làm việc với bản của mình); bản remote nằm trong bảng conflict, không ghi đè store chính. Sync tiếp theo không gửi lại cùng conflict (đã ghi nhận), nhưng vẫn sync các record khác. Mọi lựa chọn giải quyết đều tạo version mới Vn với `based_on` = Vl và `supersedes_version` = Vr, để lần sync sau phía bên kia áp dụng theo quy tắc 2 và hai bên hội tụ — không có "resolve âm thầm", mọi lựa chọn đều ghi audit.

**Màn hình conflict (spec UI bắt buộc):** màn hình "Xung đột đồng bộ" liệt kê từng conflict với: kind, tên record, diff (theo dòng cho text: SOUL/note/skill content; theo trường cho profile/todo/plan — liệt kê từng trường thay đổi, không chỉ đặt hai bản cạnh nhau), nguồn thiết bị của từng bản (tên + fingerprint rút gọn), bản nào là bản đang dùng trên thiết bị này ("Phiên bản hiện tại trên thiết bị này"), thời điểm sửa (wall-clock hiển thị). Trước khi user xác nhận, UI hiện giải thích hậu quả cho từng lựa chọn trong 4 lựa chọn: "Giữ bản của tôi — bản của peer sẽ bị thay thế bằng bản này ở lần đồng bộ sau; bản của peer vẫn xem lại được trong lịch sử xung đột."; "Lấy bản của peer — bản trên thiết bị này sẽ bị thay thế bằng bản của peer ở lần đồng bộ sau; bản hiện tại vẫn xem lại được trong lịch sử xung đột."; "Giữ cả hai — bản của peer được lưu thành record mới riêng (xem quy tắc tên dưới đây); bản trên thiết bị này giữ nguyên."; "Tôi tự sửa — mở trình chỉnh sửa với cả hai bản; bản bạn lưu sẽ thành version mới và thay thế cả hai bản ở lần đồng bộ sau." **Hoàn tác: lựa chọn giải quyết xung đột KHÔNG hoàn tác được bằng một thao tác trong v1** — sau khi xác nhận, không có nút "Hoàn tác" nào khôi phục trạng thái trước khi giải quyết; cả hai bản vẫn còn nguyên trong bảng `sync_conflicts` để xem lại và sao chép nội dung bằng tay, cho tới khi user xóa bản ghi conflict đó.

**Quy tắc tên/ID mới khi giữ cả hai:** chỉ áp dụng cho note/skill/todo/plan. Note/todo/plan: bản sao mới có `sync_id` ngẫu nhiên mới, tiêu đề = "<tiêu đề cũ> (bản từ <tên thiết bị peer>)". Skill: tên mới = "<tên cũ>__tu_<8 ký tự đầu device_id của peer>", `sync_id` = `"skill:" + tên mới`. Nếu tên mới đã tồn tại thì thêm hậu tố " (2)", " (3)", … cho tới khi duy nhất. Bản sao mới là record mới hoàn toàn (`based_on` null, version mới do thiết bị này tạo). Với SOUL và `naming_profile`, lựa chọn "Giữ cả hai" bị VÔ HIỆU (không ẩn — hiển thị mờ kèm đúng câu giải thích sau ngay tại UI): "Không thể giữ cả hai vì tên file/hồ sơ là khóa duy nhất — chỉ một bản được dùng. Hãy chọn một bản, hoặc tự sửa thành bản mới."

- **Không merge tự động ở bất kỳ đâu.** Kể cả khi diff cho thấy hai bên sửa hai chỗ khác nhau: vẫn là conflict, vẫn cần user chọn. Merge tự động (kể cả 3-way) là việc tương lai cần thiết kế riêng; Phase 7 không làm.
- **Conflict của tombstone:** một bên xóa, một bên sửa cùng record → conflict đặc biệt: UI hiện "Đã xóa ở thiết bị X, nhưng đã sửa ở đây" với 3 lựa chọn: Giữ xóa / Khôi phục bản đã sửa (hủy tombstone) / Giữ cả hai (khôi phục thành record mới, theo quy tắc tên ở trên). Không có "xóa thắng" hay "sửa thắng" mặc định.

### 4.5 Chính sách xóa: tombstone có xác nhận, không xóa âm thầm, không ngoại lệ tự động

- Xóa 1 record ở 1 phía tạo **tombstone**: record trong store chính bị xóa (hoặc đánh dấu xóa, tùy store — notebook: xóa dòng; SOUL/skill/profile: xóa file/entry), và một tombstone `{sync_id, kind, version mới, deleted_by_device, deleted_ms}` được lưu trong bảng `sync_tombstones` (`peers.db`) để lan truyền. Tombstone mang `based_on` = version của record trước khi xóa, và tham gia thuật toán §4.4 như mọi version khác.
- Phía kia nhận tombstone: KHÔNG xóa ngay. UI hiện rõ "Record X đã bị xóa ở thiết bị Y" + tuyên bố "Dữ liệu trên thiết bị này vẫn còn nguyên cho tới khi bạn xác nhận." + hai nút nhãn cụ thể: "Xóa <tên record> trên thiết bị hiện tại" và "Giữ lại và đồng bộ thành phiên bản mới" (giữ lại = tạo version mới từ bản local, hủy tombstone). Chỉ khi user bấm nút xóa thì record local mới bị xóa. **KHÔNG có ngoại lệ tự động nào**: mọi tombstone đều cần xác nhận của user ở phía nhận, kể cả khi record ở phía nhận chưa từng được sửa sau lần sync cuối. Không có đường nào áp dụng tombstone mà không qua xác nhận.
- **Giữ tombstone 90 ngày** kể từ khi tạo (`deleted_ms`), sau đó purge tự động + audit (`tombstone_purged`). Khi purge tombstone của một `sync_id`, hàng `sync_state` của `sync_id` đó với mọi peer cũng bị xóa (xem quy tắc protocol ở §4.7). Văn bản user thấy về giới hạn này (trong màn hình tombstone và docs): "Yêu cầu xóa này được giữ 90 ngày. Nếu một thiết bị không đồng bộ trong hơn 90 ngày, record đã xóa có thể xuất hiện lại như record mới khi thiết bị đó đồng bộ lại — dữ liệu không bị mất âm thầm, nhưng bạn sẽ cần xóa lại hoặc giải quyết như xung đột."
- **Resurrection sau 91 ngày (định nghĩa chính xác, mức protocol):** resurrection là việc một record đã bị xóa xuất hiện lại như record mới. Nó xảy ra khi và chỉ khi: phía nhận đã purge tombstone của `sync_id` đó (quá 90 ngày kể từ `deleted_ms` theo đồng hồ local của phía nhận), và phía gửi vẫn còn record đó (vì phía gửi đã không sync trong thời gian tombstone còn hiệu lực) và gửi nó như record thường trong phiên sync. Khi đó phía nhận không còn tombstone để từ chối, nhận record như record mới tạo (`last_seen` đặt lại từ đầu), ghi audit `record_resurrected` và hiện thông báo: "Record X đã xuất hiện lại sau hơn 90 ngày không đồng bộ. Nó từng bị xóa ở thiết bị Y. Bạn có thể giữ lại hoặc xóa lại." Nếu phía nhận VẪN CÒN tombstone (chưa tới 90 ngày), record tới với version cũ hơn hoặc bằng version tombstone thì bị loại bỏ như đã xóa, không resurrection, không conflict. Vì purge tính theo đồng hồ từng máy, hai máy có thể khác nhau về việc tombstone còn hay không — hành vi trên là hành vi theo từng máy, đã định nghĩa rõ, không mất dữ liệu âm thầm ở bất kỳ máy nào.
- Xóa ở phía gửi cũng cần xác nhận của chính user đó (xóa local vốn đã qua guard/UI của store đó); sync không thêm bước xác nhận thứ hai ở phía gửi.

### 4.6 Todos/plans: schema tối thiểu MỚI (vì cây chưa có)

Cây tại HEAD không có store todos/plans nào (§0.4). CRITERIA liệt kê todos/plans trong tập sync nhưng không định nghĩa chúng. Bản này định nghĩa schema TỐI THIỂU đủ để sync có ý nghĩa, KHÔNG xây UX quản lý task đầy đủ (không có màn hình todo, không có nhắc việc, không có scheduler — engine proactive/scheduler bị cấm ở Phase 7). Quyết định này đã chốt tại D-DESIGN7-5.

- **Lưu trữ:** 2 bảng mới trong notebook SQLite file (cùng DB với `notes`, vì cùng là dữ liệu user local-first): `todos (sync_id TEXT PK, title TEXT, done INTEGER, due_ms INTEGER NULL, notes TEXT, version_lamport INTEGER, version_device TEXT, origin_device TEXT, updated_ms INTEGER)` và `plans (sync_id TEXT PK, title TEXT, body TEXT, status TEXT, version_lamport INTEGER, version_device TEXT, origin_device TEXT, updated_ms INTEGER)`. `status` của plan là chuỗi tự do trong allow-list đóng: `active`/`paused`/`done`/`archived` — giá trị khác bị từ chối khi nhận.
- **API local tối thiểu (MỚI):** CRUD qua cùng mẫu `Notebook` (Mutex<Connection>, hàm `_for` không cần vì todos/plans không phân vùng theo agent trong v1 — mọi agent trên cùng body thấy chung; phân vùng theo agent cho todos là việc tương lai). Không có tool nào cho model sửa todos/plans trong Phase 7 (tránh model tự tạo task rác lan truyền qua sync); chỉ có CLI/UI sửa tay + sync.
- **Mở rộng schema có version (cơ chế duy nhất):** mỗi record todo/plan mang `schema_version` (số nguyên, bắt đầu từ 1) trong body. Trường mới chỉ được thêm kèm tăng `schema_version` và ghi vào thiết kế. Bên nhận có `schema_version` thấp hơn PHẢI chấp nhận record có `schema_version` cao hơn bằng cách bỏ qua trường lạ trong body đó (ngoại lệ duy nhất cho luật từ chối trường lạ ở §4.9, chỉ áp dụng cho body của todo/plan), không được drop record; bên nhận không bao giờ viết trường vượt quá `schema_version` của chính nó. Cột DB mới (nếu cần) thêm bằng migration idempotent như §4.3.
- **Hướng dẫn trường tự do:** docs và UI ghi rõ hướng dẫn cho user: "Đừng ghi mật khẩu, khóa API, hay bí mật nào vào ghi chú (notes) của todo hay nội dung (text) của plan — các trường này được đồng bộ sang thiết bị đã pair." Đây là hướng dẫn cho user, KHÔNG phải scanner quét bí mật: không có code nào quét nội dung trường tự do để chặn hay cảnh báo.
- **Sync:** todos/plans sync như mọi record khác (§4.3–§4.5, §4.7), conflict hiện diff theo trường.

### 4.7 Giao thức sync trong phiên + discovery v1 + selective sync hai cấp

**Phiên sync** (giữa hai body đã bắt tay §2.4 xong, owner trust):

```text
1. A → B: sync_offer { kinds:[...], state: { sync_id: {lamport, hash}... } }
   (state = toàn bộ record syncable của A, dạng tóm tắt hash — không gửi nội dung)
2. B so sánh với state của nó, trả sync_request { want:[sync_id...], offer:[sync_id...] }
   (want = record B thiếu/cũ hơn; offer = record B có mới hơn mà A thiếu/cũ hơn)
3. A → B: sync_push (batch, ≤100 record/batch, ≤4 MiB/batch) cho các record B muốn.
   B → A: sync_push tương tự cho các record A muốn. Hai chiều độc lập, có thể xen kẽ.
4. Mỗi batch nhận xong → sync_ack { received:[sync_id...], conflicts:[sync_id...] }.
5. Khi cả hai bên đều không còn gì để gửi → phiên kết thúc, cập nhật sync_state
   (bảng sync_state: peer_device_id + sync_id → version đã thấy + hash, theo
   định nghĩa last_seen ở §4.4 — chỉ cập nhật cho record đã áp dụng hoặc đã xác
   nhận là đã thấy, KHÔNG cập nhật cho record đang conflict), audit
   sync_session_completed { sent, received, conflicts }.
```

- **Idempotency:** nhận lại cùng record cùng version + hash thì bỏ qua (không tạo conflict giả, không ghi đè). Chỉ version concurrent theo thuật toán §4.4 mới tạo conflict; không có quy tắc "version cũ hơn thì bỏ qua" dựa trên so sánh Lamport — version đã thấy thì bỏ qua theo định nghĩa "đã thấy" ở §4.4.
- **Tombstone ở mức protocol:** tombstone lan truyền như record có `deleted: true`. Khi tombstone bị purge sau 90 ngày (§4.5), hàng `sync_state` của `sync_id` đó bị xóa cùng lúc; phiên sync sau đó coi record đó như chưa từng thấy, và resurrection xảy ra đúng như định nghĩa ở §4.5. Tombstone chưa purge thì mọi record tới có version cũ hơn hoặc bằng version tombstone đều bị loại bỏ.
- **Selective sync hai cấp (P1):** cấp 1 — loại dữ liệu: mỗi pairing có cấu hình chọn lọc LƯU LOCAL, bật/tắt theo kind (`soul_file`, `skill`, `notebook_note`, `todo`, `plan`, `naming_profile`), mặc định TẤT CẢ BẬT cho owner (vì đây là "một AI" — user pair owner là muốn đồng bộ). User tắt kind nào thì kind đó không xuất hiện trong sync_offer của máy đó (cả gửi lẫn nhận). Cấp 2 — phạm vi: notebook chỉ đồng bộ dòng Shared (cờ `shared=true`), kể cả khi kind notebook đang bật. UI hiển thị đúng câu cố định sau: "Đồng bộ Notebook chỉ bao gồm ghi chú được đánh dấu Chia sẻ. Ghi chú riêng tư không rời thiết bị." CẤM gọi tính năng này là "chọn từng record": không có chọn lọc theo từng record, không chọn lọc theo từng profile (profile sync là toàn bộ hồ sơ, không chọn từng profile).
- **Khi nào sync chạy:** (a) user bấm "Đồng bộ ngay" với 1 peer; (b) tự động khi 1 session friends mới bắt tay thành công VÀ user đã bật "tự đồng bộ khi gặp peer" (mặc định TẮT trong v1 — sync mặc định là hành động có chủ đích của user, không âm thầm). Không có lịch nền định kỳ trong v1 (không scheduler). Mỗi phiên sync đều hiện tiến trình + kết quả trong UI.
- **Discovery v1: nhập địa chỉ tay.** User nhập `IP:cổng` của peer (địa chỉ hub của peer, §5.1) vào màn hình pair/sync. Không mDNS, không broadcast, không quét LAN. Địa chỉ đã nhập cho peer đã pair được lưu trong registry (`last_addr`) để lần sau 1 lần bấm là nối lại — nhưng địa chỉ lưu KHÔNG được tin mù: bắt tay vẫn kiểm tra pin (§2.4), địa chỉ sai/chiếm đoạt chỉ dẫn tới handshake thất bại, không dẫn tới tin nhầm peer. mDNS chỉ là thiết kế tương lai (§13.2).
- **Giới hạn phiên:** tối đa 1 phiên sync đang chạy với 1 peer tại 1 thời điểm; timeout phiên 5 phút (sync lớn hơn thì chia nhiều phiên — user bấm lại, state đã lưu nên phiên sau tiếp tục từ chỗ dở, không gửi lại từ đầu nhờ sync_state + idempotency).

### 4.8 Kiểm chứng (sync)

- `sync_two_way_two_instances`: 2 instance thật, mỗi bên tạo record riêng + sửa record chung không xung đột → sync 2 chiều → cả hai bên có đủ record, nội dung đúng.
- `sync_concurrent_detection_successor_applies`: theo worked example 1 (§4.4): B sửa từ version chung → A áp dụng, không conflict, `last_seen` cập nhật.
- `sync_concurrent_detection_concurrent_conflicts`: theo worked example 2 (§4.4): hai bên sửa từ cùng baseline → conflict ở cả hai bên, cả hai bản nguyên vẹn, `last_seen` giữ nguyên.
- `sync_concurrent_detection_lamport_larger_does_not_win`: A sửa hai lần (Lamport 7) từ baseline, B sửa một lần (Lamport 6) từ cùng baseline → vẫn concurrent, vẫn conflict; bản của B không bị ghi đè dù Lamport nhỏ hơn.
- `sync_concurrent_detection_missing_history_is_conflict`: xóa `peers.db` (mất `sync_state`) rồi sync lại → mọi record khác nhau đều thành conflict, không bản nào bị ghi đè hay mất.
- `sync_conflict_resolution_converges`: resolve conflict bằng một lựa chọn → version giải quyết mang `supersedes_version` → sync lại → phía bên kia áp dụng, hai bên hội tụ, không conflict lại.
- `sync_conflict_keeps_both`: sửa đồng thời cùng note ở 2 bên (không sync giữa chừng) → sync → conflict record xuất hiện ở cả hai bên, CẢ HAI bản nguyên vẹn, không bản nào bị ghi đè; resolve bằng "giữ bản của tôi" → sync lại → hội tụ.
- `sync_tombstone_requires_confirmation`: xóa 1 bên → sync → bên kia thấy thông báo xóa, record VẪN CÒN cho tới khi user xác nhận; xác nhận → xóa; chọn giữ lại → record sống lại, sync lại không xóa nữa.
- `sync_tombstone_no_automatic_exception`: record chưa từng sửa sau lần sync cuối mà nhận tombstone → vẫn cần xác nhận, không tự xóa.
- `sync_tombstone_resurrection_after_91_days`: tombstone quá 90 ngày (đồng hồ test) → purge + xóa `sync_state` → peer còn record sync lại → record xuất hiện lại như mới + audit `record_resurrected` + thông báo; trước 90 ngày thì record cũ bị loại bỏ, không resurrection.
- `sync_canary_secret_never_in_payload`: đặt secret sentinel vào vault file, notebook private, history giả, audit; chạy sync đầy đủ; đọc record sync thô (bắt ở tầng serializer + bắt gói loopback) → sentinel không xuất hiện ở đâu.
- `sync_private_notes_never_leave`: note private ở A → sync → B không có note đó, không thấy tên note đó trong sync_offer (offer chỉ mang sync_id + hash của record shared — và sync_id của private không xuất hiện).
- `sync_unpaired_peer_cannot_read`: peer chưa pair bắt được payload sync (harness đưa ciphertext cho peer lạ) → giải mã fail (khóa phiên khác).
- `sync_profile_missing_backend_drops`: profile sync tới máy không có backend cùng tên → profile bị drop + warning, không tạo backend, không lỗi app.
- `sync_selective_kind_off`: tắt kind `skill` ở 1 bên → skill không di chuyển chiều nào; các kind khác vẫn sync.
- `sync_selective_scope_shared_only`: kind notebook bật nhưng note private → không sync; note shared → sync; UI hiển thị đúng câu cố định về notebook Shared.
- `sync_idempotent_replay`: chạy cùng phiên sync 2 lần → lần 2 không tạo conflict giả, không ghi đè, số record gửi = 0.
- `sync_record_size_cap`: record > 256 KiB → từ chối sync với thông báo rõ, không cắt, các record khác vẫn sync.
- `sync_tombstone_90_day_purge`: tombstone quá 90 ngày (đồng hồ test) → purge + audit; sync sau đó không mang tombstone đó.

### 4.9 Schema record theo kind (plaintext JSON trong payload đã mã hóa)

Mọi record đều có envelope chung + `body` theo kind. Deserializer từ chối mọi trường ngoài schema (allow-list trường, như `deny_unknown_fields` của config), trừ ngoại lệ mở rộng schema của todo/plan ở §4.6. `body` dưới đây là ví dụ minh họa, không phải fixture cố định.

Envelope chung:

```json
{
  "sync_id": "<hex 32, hoặc 'soul:<file>' / 'skill:<name>' / 'profile:<name>'>",
  "kind": "soul_file",
  "version_lamport": 42,
  "version_device": "<device_id>",
  "based_on": "{ lamport: 41, device: '<device_id>' } | null",
  "supersedes_version": "{ lamport, device } | null",
  "origin_device": "<device_id>",
  "updated_ms": 0,
  "deleted": false,
  "body_sha256": "<hex — SHA-256 của canonical JSON của body>",
  "body": {}
}
```

`body` theo kind:

- `soul_file`: `{ "file": "SOUL.md", "format_version": 1, "text": "<toàn bộ body SOUL>" }`. Phía nhận validate tên file + `parse_soul_file` trước khi ghi.
- `skill`: `{ "name": "<tên skill>", "skill_json": { "...SkillDef đã serialize như file store..." } }`. Phía nhận validate `validate_shape` + `content_hash` + status phải là `promoted` (status khác → từ chối record).
- `notebook_note`: `{ "title": "...", "text": "...", "created_ms": 0, "shared": true }`. `shared` phải là `true` trong record sync — record mang `shared:false` bị từ chối (không bao giờ nhận note private qua sync, kể cả khi peer gửi lén).
- `todo`: `{ "schema_version": 1, "title": "...", "done": false, "due_ms": null, "notes": "..." }`.
- `plan`: `{ "schema_version": 1, "title": "...", "text": "...", "status": "active" }` (`status` trong allow-list §4.6).
- `naming_profile`: `{ "profile": { "...ProfileEntry đã serialize..." } }`. Phía nhận chạy `ProfileSet::resolve` như đọc config; profile lỗi → drop + warning, không lỗi phiên sync.

Tombstone là envelope chung với `deleted: true` và `body: null` (không mang nội dung cũ). `body_sha256` của tombstone là SHA-256 của chuỗi rỗng cố định (quy ước, ghi trong code) — phía nhận không kiểm tra hash nội dung của tombstone, chỉ kiểm tra version + chữ ký phiên (AEAD đã bảo vệ toàn bộ payload).

---

## 5. Hub server xác thực + mã hóa (P7.0 mục 5, P7.3 / P0-18)

### 5.1 Quyết định: loopback mặc định, LAN chỉ khi bật rõ + có pairing

- Hub là tiến trình Mimint desktop lắng nghe TCP để phục vụ: (a) friends protocol (§2) từ peer đã pair; (b) driver điều khiển body từ xa (method body qua kênh đã auth); (c) OTA + voice cho đường XiaoZhi (§8, cùng tiến trình, cổng riêng — xem §8.2). Một tiến trình, một não (§8.1), nhiều listener theo cấu hình.
- **Mặc định: chỉ loopback `127.0.0.1`.** Cổng friends mặc định `19120` (số mới do bản này chọn, không trùng cổng nào trong cây; andee dùng 9008/9120 — không dùng các số đó để tránh nhầm lẫn và tránh va chạm nếu andee chạy cùng máy). Cổng cấu hình được. Loopback-only là trạng thái khi user chưa bật gì: 2 instance trên cùng máy vẫn pair/sync được qua loopback (đây cũng là cách test G2 chạy).
- **Bật LAN:** user phải bật tường minh trong cấu hình (`[hub] listen_lan = true`) VÀ hub phải có ít nhất 1 pairing (hoặc đang ở chế độ pairing §3.3). Bật LAN mà chưa pair thiết bị nào → hub vẫn listen nhưng từ chối mọi kết nối điều khiển/sync (chỉ chấp nhận `pair_request` khi đang ở chế độ pairing). UI hiển thị rõ trạng thái phơi nhiễm của hub theo đúng 1 trong 5 trạng thái sau, không dùng nhãn chung "Hub đang mở": (1) "Loopback — chỉ thiết bị hiện tại truy cập"; (2) "LAN đang bật — có thể nhận kết nối từ mạng nội bộ"; (3) "Pairing mode — đang cho phép yêu cầu ghép đôi"; (4) "Driver đang kết nối — thiết bị khác đang có phiên điều khiển"; (5) "LAN chưa pair — không chấp nhận điều khiển/sync từ peer lạ". Khi nhiều trạng thái cùng đúng (ví dụ LAN bật + driver đang kết nối), UI hiển thị trạng thái phơi nhiễm cao nhất kèm các trạng thái phụ, không gộp thành một nhãn mơ hồ.
- **Nút "Đóng LAN ngay":** về loopback ngay, không cần restart — listener LAN đóng, session LAN đang mở bị cắt. Khi có session đang hoạt động, nút mở hộp xác nhận liệt kê đúng các session sẽ bị đóng (tên thiết bị + loại session + thời điểm bắt đầu) và chỉ đóng sau khi user xác nhận. Sau khi đóng, UI xác nhận trạng thái mới ("Đã về loopback — không còn nhận kết nối từ LAN"). Tuyên bố bắt buộc kèm nút: "Đóng LAN không thu hồi pairing — các thiết bị đã pair vẫn còn trong danh sách và nối lại được khi bạn bật LAN."
- **CẤM TUYỆT ĐỐI bind `0.0.0.0` không auth** (lỗi BodyWsServer của andee — PROPOSALS §6). Không có cấu hình nào, không có flag debug nào, không có bản build nào của Mimint được bind không-auth. Code hub không có đường nào chấp nhận kết nối mà chưa qua handshake §2.4 (trừ `pair_request` trong chế độ pairing, và OTA XiaoZhi theo §8.2 với giới hạn riêng đã ghi). Test quét §5.5 ép điều này.

### 5.2 Auth: xác thực lẫn nhau bằng khóa danh tính đã pin + ECDH ephemeral + AEAD (D-DESIGN7-2)

CRITERIA P7.3 viết "mTLS với cert đã pin"; bản thiết kế này diễn giải cụm đó thành mutual authentication bằng khóa danh tính đã pin + ECDH ephemeral + AEAD như dưới đây, và không dùng từ đó ở bất kỳ nơi nào khác ngoài câu này và nhật ký độ lệch ở §14. (Từ đây trong bản này, dạng rút gọn "xác thực lẫn nhau bằng khóa đã pin" mang đúng nghĩa đầy đủ của cụm trên.)

- **Friends/sync + driver qua friends protocol: xác thực lẫn nhau bằng khóa danh tính P-256 đã pin + chữ ký transcript (§2.4), kênh mã hóa AEAD.** Không dùng X.509/TLS cho đường này. Lý do bắt buộc: cây không có công cụ sinh chứng chỉ (probe c5: rcgen 0 hit; cert duy nhất trong cây là fixture test commit sẵn), và sinh cert X.509 bằng tay (DER tự viết) là việc lớn, dễ sai, không cần thiết khi pin khóa thô đạt cùng tính chất với ít code hơn. "Tái dùng cơ chế pinning Phase 4" được hiểu là tái dùng MÔ HÌNH pin (SHA-256 pin thay PKI, pin từ pairing ngoài băng, không fallback) + tái dùng đúng code `PinnedVerifier` ở nơi peer đã có TLS (xem dưới) — không phải ép X.509 vào mọi kết nối.
- **Nơi peer đã có TLS thì dùng TLS + pinning Phase 4 nguyên vẹn:** Android shell (cert tự ký do shell sinh, client pin SHA-256 DER — `pinned_client_config` `android.rs:394`) và firmware ESP32 đường v2 (ESP-IDF TLS + pinning phía firmware — §8.3). Ở các đường này, hub (phía client) tái dùng `PinnedVerifier`/`fingerprint_of_cert_der` như-is.
- **REMOVED khỏi v1 — đường token driver:** đường xác thực driver không-phải-friends bằng pairing token + session binding (kiểu Android) đã bị CẮT khỏi v1 theo D-DESIGN7-2. Lý do: không có use case nào trong CRITERIA cần nó; một mô hình danh tính duy nhất (friends đã pair) dễ giải thích và ít bề mặt tấn công hơn. Không có cấu hình nào, không có file token nào, không có đường code nào chấp nhận token driver trong v1; request mang token kiểu cũ bị từ chối như chưa auth (xem test §5.5). Harness kiểm thử và mọi client điều khiển trong Phase 7 đều dùng danh tính friends đã pair, không có ngoại lệ.
- **Không có endpoint nào chấp nhận kết nối không-auth ở bản release.** Kể cả endpoint health/ping: `ping` chỉ tồn tại SAU handshake (§2.5). Endpoint duy nhất chấp nhận message trước auth là `pair_request` (chỉ trong chế độ pairing, rate-limited, không trả secret) và OTA XiaoZhi (§8.2, LAN + allow-list UUID + không firmware). Test quét §5.5 liệt kê mọi route/listener và khẳng định điều này.

### 5.3 Allow-list theo vai trò + đèn báo + nút ngắt

- **Allow-list driver (server-side enforce, deny-by-default):** hub định nghĩa tập method điều khiển cho driver từ xa. Method = các action body (click/type/key/screenshot/...) + method hub (sync, approve-relay, handoff-relay). Phân quyền:

| Method nhóm | Owner (pair owner) | Guest (pair guest) | Chưa auth |
|---|---|---|---|
| Đọc màn hình (screenshot, display_size, focused_window, permissions) | Có | Có | KHÔNG |
| Gửi input (click, type, key, scroll, drag, ...) | Có | KHÔNG | KHÔNG |
| Sync / approve / handoff | Có | KHÔNG | KHÔNG |
| Quản lý hub (thu hồi, xoay key, đổi cấu hình) | KHÔNG (chỉ local UI) | KHÔNG | KHÔNG |
| Kill/stop body | Có (owner) + nút ngắt local luôn có | KHÔNG | KHÔNG |

- Mọi method ngoài bảng → `unknown_method`/`forbidden_role` + audit. Method mới trong tương lai mặc định KHÔNG có trong allow-list (deny-by-default, ngược với TurnGuards deny-list của andee — đúng hướng đã chốt ở SYNTHESIS §6).
- **Đèn báo driver từ xa:** khi có session driver từ xa đang hoạt động (bất kỳ method nào ngoài ping trong 60 giây qua), UI hiển thị đèn báo rõ ràng: "Thiết bị <tên> đang điều khiển máy này" + thời điểm action cuối + trust level. Đèn báo không bao giờ tắt âm thầm khi session còn mở. CLI in trạng thái tương đương.
- **Nút ngắt ngay:** nút "Ngắt driver từ xa" trong UI (và phím tắt khẩn cấp hiện có của body, nếu có) → đóng TẤT CẢ session driver từ xa ngay, audit, đèn báo tắt. Ngắt không thu hồi pairing (pair lại bằng cách nối lại); muốn cắt vĩnh viễn thì Thu hồi (§3.6). Nút ngắt hoạt động cả khi driver đang giữa action (action đang chạy trong actor thì theo luật hủy của actor — không giật giữa action, nhưng không nhận action mới nào từ driver đó).
- **Driver từ xa đi qua BodyActor hiện có** (§0.4): không có đường thứ hai lái body. Mọi action từ driver đều qua cùng hàng đợi + timeout + latch `broken` như action local. Driver không bypass guard: action consequential từ driver vẫn qua approver local (thẻ phê duyệt hiện trên máy hub, ghi rõ "yêu cầu từ thiết bị X") — driver từ xa không tự phê duyệt cho chính nó.

### 5.4 Giới hạn vận hành của hub

- Tối đa **4 session friends đồng thời** (4 body) + tối đa **8 kết nối TCP đang chờ handshake** (chống mở kết nối ồ ạt — vượt thì từ chối kết nối mới ngay, không xếp hàng dài). Timeout handshake 10 giây (§2.4); timeout idle session 10 phút không message → đóng (peer nối lại khi cần; không có keep-alive âm thầm giữ session mãi). Các giới hạn này là giới hạn chống DoS mà §2.4 trỏ tới; frame/batch cap nằm ở §2.2.
- Hub chạy trên thread/task riêng, không chặn UI thread, không chặn worker lượt. Lỗi hub (bind thất bại, accept lỗi) ghi audit + hiện trạng thái trong UI, không làm sập app, không làm chết lượt đang chạy.
- **Không có endpoint debug.** Không có route nào trả về trạng thái nội bộ, log, config, hay danh sách peer mà không qua auth + owner trust. Route duy nhất trả về thông tin trước auth là không có route nào (kể cả version banner — handshake HELLO không mang banner, chỉ mang version số).

### 5.5 Kiểm chứng (hub)

- `hub_loopback_by_default`: cấu hình trống → hub chỉ bind loopback (kiểm tra bằng cách nối từ địa chỉ LAN → thất bại; nối loopback → được).
- `hub_lan_requires_explicit_opt_in`: bật LAN trong cấu hình → listen LAN; tắt → không. Không có đường nào khác bật LAN (không env var lén, không flag CLI lén).
- `hub_no_auth_no_control`: kết nối TCP thô, gửi method điều khiển không handshake → từ chối, không action nào chạy, audit ghi.
- `hub_token_path_removed`: gửi request mang token kiểu cũ (đường driver đã cắt ở §5.2) → bị từ chối như chưa auth + audit; không có cấu hình hay file token nào được đọc; grep code hub không còn đường xác thực bằng token.
- `hub_revoked_device_rejected`: thiết bị đã revoke nối lại → từ chối ở handshake, không tới được method nào.
- `hub_replay_rejected`: gửi lại request điều khiển cũ đã mã hóa (bắt từ session trước) → seq/khóa sai → từ chối, không action nào chạy lại.
- `hub_no_unauthenticated_routes_scan`: test cổng quét — liệt kê mọi listener/route của hub trong code (bằng review có checklist + test nối thô tới từng cổng với request không-auth) → mọi route đều từ chối trừ `pair_request` trong chế độ pairing.
- `hub_remote_driver_indicator_and_kill`: driver từ xa gửi action → đèn báo hiện đúng tên thiết bị; bấm ngắt → session đóng, action mới bị từ chối, đèn tắt.
- `hub_driver_goes_through_actor`: driver gửi action trong lúc actor `broken` → nhận `UnknownOutcome` như local, không có đường bypass.
- `hub_close_lan_lists_sessions_and_keeps_pairing`: bấm Đóng LAN khi có session đang mở → hộp xác nhận liệt kê đúng session sẽ đóng; xác nhận → listener LAN đóng, session cắt, pairing trong registry còn nguyên, UI về trạng thái loopback.

---

## 6. Phê duyệt xuyên thiết bị (P7.0 mục 6, P7.4 / P0-13)

### 6.1 Ma trận tin cậy kênh (P0-13)

| Kênh | Được làm gì | Không được làm gì |
|---|---|---|
| App Mimint trên máy hub (kênh chính) | Mọi thứ: tạo thẻ, trả lời thẻ, quản lý thiết bị, cấu hình | — |
| Peer đã pair (owner) qua kênh E2E | Trả lời thẻ ĐANG CHỜ do hub gửi tới (xem §6.2) | Không tự tạo thẻ, không phê duyệt hộ thẻ của agent khác, không sửa thẻ, không batch |
| Peer guest | Không phê duyệt gì | Mọi quyết định từ guest đều bị từ chối |
| Notification (thông báo OS) | Chỉ gợi ý: "có thẻ đang chờ, mở app để trả lời" | Không trả lời từ notification (không có action allow/deny trên notification trong v1) |
| Mọi kênh khác (bot chat, email, web) | Không có trong Phase 7 | — |

App là kênh chính: peer chỉ là "tay nối dài" của thẻ đang chờ trên hub. Nếu peer mất kết nối giữa chừng, thẻ vẫn chờ trên hub tới hết 60 giây như local — peer không phải điểm sống còn.

### 6.2 Quyết định: binding bộ ba + timeout 60 giây giữ nguyên

- Khi 1 thẻ phê duyệt được tạo trên hub (qua `GuardedExecutor` như hôm nay) VÀ có peer owner đang nối session VÀ user đã bật "cho phép phê duyệt từ peer" cho peer đó, hub gửi `approval_card` qua session E2E tới peer: `{ device_id_hub, agent_id, request_id, tool, args (immutable, như thẻ local), context, created_ms, deadline_ms }`. Công tắc này mặc định TẮT theo từng peer (D-DESIGN7-7): user bật rõ ràng cho từng peer trong UI quản lý thiết bị ("Cho phép thiết bị này trả lời thẻ phê duyệt"). Lý do: phê duyệt từ xa là quyền mạnh; pair owner là để sync, chưa chắc user muốn peer đó bấm Allow hộ.
- **Thẻ phê duyệt từ peer phải đủ trường nguồn gốc.** Peer hiển thị thẻ với đầy đủ: tên hub gửi thẻ, tên thiết bị đang trả lời (chính thiết bị này), agent liên quan, thời gian còn lại (đếm ngược tới deadline), hành động + tham số đầy đủ (không rút gọn tham số), và dòng nguồn "Thẻ từ thiết bị <tên hub>". Phân biệt thẻ peer với thẻ local bằng chữ và trường nguồn gốc, không chỉ dựa vào màu sắc hay biểu tượng.
- Peer trả lời bằng `approval_decision`: `{ device_id_peer, agent_id, request_id, decision: allow|deny }`. Hub kiểm tra, theo thứ tự: (1) session đã xác thực + peer là owner + peer được bật phê duyệt; (2) `(device_id, agent_id, request_id)` khớp đúng thẻ đang chờ (thẻ đã trả lời/hết hạn/bị hủy thì quyết định bị từ chối — dùng 1 lần); (3) quyết định đến trước `deadline_ms` (60 giây từ lúc tạo thẻ, cùng đồng hồ hub — không tin timestamp của peer); (4) `agent_id` trong quyết định khớp agent của thẻ (chống phê duyệt hộ thẻ agent khác). Mọi kiểm tra đều fail-closed → deny.
- **Binding:** executor local vẫn kiểm tra cặp `(agent_id, request_id)` như hôm nay (`guards.rs`); kiểm tra `device_id` + "thẻ đang chờ" + deadline là lớp NGOÀI (hub approval relay) trước khi quyết định tới executor. Executor không cần biết quyết định đến từ đâu — nhưng audit thì biết (§6.3). Không sửa ngữ nghĩa `GuardedExecutor`, không sửa `ApprovalOutcome`.
- **Timeout 60 giây giữ nguyên** (`DEFAULT_APPROVAL_TIMEOUT`). Hết hạn → deny như local, kể cả khi peer đã bấm Allow nhưng quyết định tới muộn. Không gia hạn vì "peer đang trả lời". Không batch: 1 quyết định chỉ trả lời đúng 1 thẻ; không có "approve tất cả", không có "luôn cho phép".
- **Peer không tự tạo thẻ:** `approval_card` chỉ đi từ hub → peer; peer gửi `approval_card` → bị từ chối + audit. Peer không gửi được quyết định cho thẻ không tồn tại. Peer không thấy thẻ của agent khác ngoài thẻ hub gửi cho nó (hub chỉ gửi thẻ của agent đang chạy lượt mà peer được phép trả lời — trong v1: mọi thẻ của mọi agent trên hub, vì peer owner là chính user; phân biệt theo agent là việc tương lai nếu cần).
- **Trả lời sau khi agent bị hủy → deny**, như luật local (§3.1 Phase 6): relay kiểm tra token hủy của agent trước khi chuyển quyết định tới executor.
- **Mất kết nối giữa chừng:** peer bấm Allow nhưng session đứt trước khi quyết định tới hub → thẻ chờ tới hết hạn → deny. Không retry quyết định tự động (retry quyết định = replay). User bấm lại trên hub nếu cần.

### 6.3 Audit attribution theo thiết bị

- Mọi quyết định phê duyệt (local hay từ peer) đều ghi audit qua đường hiện có, với `detail` bổ sung: `deciding_device_id` (device_id của thiết bị đã quyết; hub local ghi device_id của chính hub), `deciding_device_name` (tên hiển thị), `decision_channel` (`local` | `peer`). Không thêm trường first-class mới vào `AuditRecord` trong Phase 7 (tránh migration schema audit; `detail` đã là nơi mang context — tiền lệ `profile` trong `detail` trước Phase 6). Quyết định ghi trong `detail` đã chốt tại D-DESIGN7-6.
- Quyết định bị từ chối ở lớp relay (sai binding, hết hạn, guest, replay) cũng ghi audit với `decision: denied` + lý do, để user thấy được "có ai đó đang gửi quyết định lạ".

### 6.4 Kiểm chứng (phê duyệt xuyên thiết bị)

- `approve_from_peer_runs_action`: peer thật trả lời Allow → action chạy, audit đúng `deciding_device_id` + channel `peer`.
- `deny_from_peer_blocks_action`: peer trả lời Deny → action không chạy.
- `approve_replay_rejected`: gửi lại quyết định cũ (cùng request_id) → từ chối, không action nào chạy lần hai.
- `approve_cross_agent_rejected`: quyết định mang agent_id khác với thẻ → từ chối (`approval_id_mismatch` ở lớp executor hoặc relay — cùng lý do, cùng deny).
- `approve_cross_device_replay_rejected`: quyết định của peer A gửi lại qua session của peer B (device_id lệch) → từ chối.
- `approve_after_expiry_denied`: quyết định tới sau 60 giây → deny, action không chạy.
- `approve_guest_rejected`: guest gửi quyết định → từ chối.
- `approve_peer_cannot_create_card`: peer gửi `approval_card` → từ chối + audit.
- `approve_no_batch`: 1 quyết định không trả lời được 2 thẻ (thẻ thứ hai vẫn chờ tới hết hạn).
- `approve_requires_explicit_peer_opt_in`: peer chưa được bật "trả lời thẻ" → hub không gửi thẻ tới peer đó, quyết định từ peer đó (nếu gửi lén) bị từ chối.
- `approve_card_shows_full_origin`: thẻ hiển thị trên peer có đủ tên hub gửi, tên thiết bị trả lời, agent, thời gian còn lại, hành động + tham số đầy đủ; thẻ peer phân biệt được với thẻ local bằng chữ, không chỉ màu/biểu tượng.

---

## 7. Handoff xuyên thiết bị (P7.0 mục 7, P7.6 / P0-1 có phạm vi)

### 7.1 Gói handoff gồm gì

Gói handoff là một JSON (đi trong message `handoff_offer` đã mã hóa E2E, §2.5), gồm ĐÚNG các trường sau — không hơn:

```json
{
  "handoff_id": "<hex 32 — id duy nhất của lần handoff>",
  "from_device": "<device_id>",
  "to_device": "<device_id>",
  "task_text": "<mô tả task bằng lời user/model>",
  "steps_done": ["<bước đã xong 1>", "<bước đã xong 2>"],
  "next_step": "<bước tiếp theo, mô tả rõ>",
  "file_refs": ["<đường dẫn file/tên file liên quan — chỉ tham chiếu, không nội dung>"],
  "tab_refs": ["<mô tả tab/cửa sổ liên quan — chỉ tham chiếu>"],
  "notebook_refs": ["<sync_id của note dòng shared liên quan>"],
  "lease": { "lease_id": "<hex 32>", "actuator": "body", "expires_ms": 0 },
  "package_version": 1,
  "created_ms": 0,
  "signature": "<hex — ECDSA của from_device lên canonical JSON không có trường signature>"
}
```

- **Không kèm:** secret/vault/key, nội dung file (chỉ tham chiếu đường dẫn — file phải tự có trên máy nhận qua đường khác, ví dụ sync hoặc ổ chung; handoff không truyền file), history chat, nội dung note private (chỉ `sync_id` của note shared — máy nhận đọc note đó từ notebook đã sync của nó; nếu chưa sync thì handoff ghi rõ "thiếu note X, sync trước đã"), screenshot, clipboard.
- **Notebook context chỉ dòng shared:** `notebook_refs` chỉ được mang `sync_id` của note `shared=true`; serializer handoff kiểm tra cờ shared như serializer sync (§4.2). Note private trong context → handoff bị từ chối tạo (không lặng lẽ bỏ qua — user phải biết context bị thiếu).
- **Kích thước gói:** ≤ 64 KiB. Vượt → từ chối tạo (task text quá dài thì user tóm tắt lại; không cắt âm thầm).

### 7.2 Lease 1 actuator + thanh trạng thái handoff cố định

- **Một actuator, một lease.** Actuator ở đây là body của máy nhận (chuột/bàn phím/màn hình). Tại 1 thời điểm, tối đa 1 body giữ lease điều khiển cho 1 task handoff. Lease gồm: `lease_id`, `actuator:"body"`, `holder_device`, `expires_ms` (TTL mặc định **10 phút** kể từ khi nhận handoff; gia hạn bằng cách handoff lại, không có message gia hạn riêng trong v1).
- **Máy gửi giữ lease cho tới khi máy nhận chấp nhận.** Khi user khởi xướng handoff trên A: A tạo gói + lease (A đang giữ), gửi `handoff_offer` tới B, và A NGỪNG hành động trên task đó ngay (không chạy thêm bước nào — tránh 2 body cùng làm). A hiển thị "Đã gửi handoff, đang chờ B nhận" và nói rõ user chưa mất task: quyền thực thi đang chờ chuyển giao, task và dữ liệu vẫn nguyên.
- **Máy nhận xác minh rồi mới tiếp tục** (§7.3). Chấp nhận → B giữ lease, tiếp tục từ `next_step` (tạo lượt mới với task text + context, qua brain/guard như lượt thường — handoff không bypass guard/approver). Từ chối → lease trả về A (A hiển thị "B đã từ chối, bạn tiếp tục"), A được hành động lại.
- **Lease hết hạn → trả về.** Hết TTL mà B chưa xong/không phản hồi → lease hết hiệu lực, B không được hành động tiếp trên task đó (hành động sau hết hạn bị từ chối ở lớp lease-check trước khi vào actor), A được hiển thị "lease đã hết hạn, bạn có thể tiếp tục hoặc handoff lại". Không có 2 body cùng hành động: tại mọi thời điểm, tối đa 1 bên tin rằng mình giữ lease (bên kia hoặc đã nhận từ chối/hết hạn, hoặc đang chờ).
- **Lease tranh chấp:** B chỉ giữ tối đa 1 lease đang hoạt động cho actuator `body` tại 1 thời điểm (mọi handoff khác tới B trong lúc đó bị từ chối bằng `handoff_reject` mang lý do `lease_busy` cho tới khi lease hiện tại hết hạn/hoàn thành/trả về). Đơn giản, tránh 2 task cùng lái 1 body.
- **Lease không sống qua restart tiến trình (quyết rõ).** Lease lưu in-memory trên cả hai phía; chết tiến trình → lease mất. Phía còn lại không nhận được thông báo nào mà chỉ thấy lease hết hạn theo TTL, rồi mới được hành động lại hoặc handoff lại; muốn tiếp tục task sau restart thì handoff lại từ đầu (gói mới, lease mới). Không khôi phục lease từ đĩa, không khôi phục lease sau restart.
- **Ack từ phía nhận sau hành động.** B gửi `handoff_accept` ngay khi user bấm "Tiếp tục ở đây" (trước khi hành động) — đó là ack chuyển lease; A chỉ coi lease đã chuyển khi nhận `handoff_accept` hoặc khi hết TTL. Khi B xong task hoặc dừng task giữa chừng, B gửi `handoff_complete` (§2.5) — đó là ack sau hành động; A coi lease đã trả về khi nhận `handoff_complete`, `handoff_reject`, hoặc khi hết TTL. Không có ack theo từng bước hành động trong v1: lease + TTL là cơ chế duy nhất ngăn hai bên cùng hành động giữa chừng.
- **Mất kết nối giữa chừng.** A mất kết nối sau khi gửi offer → A vẫn không được hành động cho tới khi hết TTL, và thanh trạng thái chuyển sang "không xác định" (xem dưới). B mất kết nối giữa chừng trong lúc giữ lease → B được tiếp tục tới hết TTL (nó giữ lease); A thấy "không xác định" tới hết TTL rồi mới được tiếp tục hoặc handoff lại. Nối lại không tự tiếp tục handoff cũ: muốn tiếp tục thì handoff lại.
- **Thanh trạng thái handoff cố định (spec UI bắt buộc).** Cả hai phía hiển thị một thanh trạng thái cố định trong suốt handoff, gồm: tên task, thiết bị giữ quyền điều khiển, thời gian còn lại của lease (đếm ngược), và đúng 1 trong 7 trạng thái: "Chưa bàn giao" / "Đang chờ chấp nhận" / "Đã bàn giao" / "Đang giữ quyền điều khiển" / "Bị từ chối" / "Hết hạn" / "Không xác định được trạng thái". Mất kết nối → trạng thái "Không xác định được trạng thái", không mặc định rằng quyền đã chuyển thành công. CẤM chỉ hiện "Handoff thành công" mà không có người giữ quyền + thời gian còn lại + trạng thái trong 7 trạng thái trên.

### 7.3 Receiver xác minh gì trước khi tiếp tục

B (máy nhận) kiểm tra theo thứ tự, fail-closed ở bước đầu tiên sai:

1. Session đã xác thực + peer là owner (§2.4, §3.5). Guest không gửi/nhận handoff.
2. `to_device` == device_id của B. Gói gửi nhầm máy → từ chối.
3. `package_version` == 1. Version khác → từ chối (không có tương thích ngược trong v1).
4. Chữ ký ECDSA trên canonical JSON (không có trường `signature`) verify được bằng public key ĐÃ PIN của `from_device`. Sai → từ chối + audit `handoff_forged`. (Chữ ký cần thiết vì gói handoff có thể được lưu/chuyển tiếp — CRITERIA yêu cầu xác minh chữ ký/khóa + version; kênh E2E một mình không đủ cho tính chất "gói này do đúng peer đó tạo, không bị sửa sau khi tạo".)
5. Lease còn hạn, với dung sai lệch giờ 2 phút: B coi lease hết hạn khi giờ hiện tại của B cộng 2 phút đã tới hoặc vượt `expires_ms` (tính bảo thủ — B hết hạn sớm 2 phút để không hành động sau khi A đã coi lease hết hạn). `actuator` == `"body"`, B không giữ lease nào khác đang hoạt động. Nếu lệch giờ giữa hai máy vượt quá 2 phút (phát hiện qua `created_ms`/`expires_ms` lệch xa giờ local của B), B từ chối gói với lý do rõ "lệch giờ giữa hai thiết bị quá 2 phút — hãy đồng bộ đồng hồ rồi handoff lại", audit ghi rõ; không tự hiệu chỉnh đồng hồ, không fail-open.
6. `notebook_refs` đều là note shared và đều có trong notebook của B (đã sync). Thiếu note → từ chối với lý do rõ "thiếu context, sync trước đã" (không tiếp tục mù).
7. Không có secret trong gói: kiểm tra các trường không chứa pattern secret đã biết? — KHÔNG: kiểm tra pattern là việc không đáng tin. Thay vào đó: serializer phía gửi chỉ cho phép đúng các trường §7.1 (allow-list), và deserializer phía nhận từ chối mọi trường lạ. Không có kiểm tra nội dung heuristic nào.

Sau khi qua 7 bước, B hiển thị gói cho user xem (task, bước đã xong, bước tiếp theo, tham chiếu) + nút "Tiếp tục ở đây" / "Từ chối". Chỉ khi user bấm Tiếp tục thì B mới tạo lượt mới và gửi `handoff_accept`. Handoff không tự chạy khi nhận được gói — human-present như mọi computer use.

### 7.4 Kiểm chứng (handoff)

- `handoff_two_instances_continues_next_step`: 2 instance thật, A làm 2 bước, handoff → B tiếp tục đúng `next_step`, không làm lại bước đã xong, không bỏ bước.
- `handoff_lease_conflict_second_rejected`: B đang giữ lease → C gửi handoff khác → bị từ chối `lease_busy`; sau khi lease hết hạn → C gửi lại được.
- `handoff_forged_package_rejected`: gói không ký / ký sai / ký bằng key khác → từ chối, không lượt nào được tạo, audit `handoff_forged`.
- `handoff_wrong_recipient_rejected`: gói `to_device` khác → từ chối.
- `handoff_expired_lease_rejected`: lease hết hạn (đồng hồ test) → từ chối; B không hành động sau hết hạn.
- `handoff_no_secret_in_package`: gói chứa trường lạ (ví dụ `api_key`) → deserializer từ chối toàn bộ gói; gói chứa `notebook_refs` trỏ note private → phía gửi từ chối tạo.
- `handoff_reject_returns_lease`: B từ chối → A được hành động lại, lease về A, UI cả hai bên hiển thị đúng.
- `handoff_sender_stops_after_offer`: A gửi offer → A không chạy thêm bước nào trên task đó cho tới khi B từ chối/hết hạn (kiểm tra bằng log action của A).
- `handoff_lease_lost_on_restart`: B giữ lease rồi restart tiến trình → lease mất; A thấy hết hạn theo TTL rồi mới được hành động lại; handoff lại từ đầu thì chạy được.
- `handoff_clock_skew_beyond_tolerance_rejected`: lệch giờ hai máy hơn 2 phút (đồng hồ test) → B từ chối gói với lý do lệch giờ; lệch trong 2 phút → B tính bảo thủ, hết hạn sớm.
- `handoff_complete_ack_returns_lease`: B xong task gửi `handoff_complete` → A thấy lease trả về ngay, không cần chờ hết TTL; thanh trạng thái cả hai bên hiện đúng trạng thái + người giữ quyền + thời gian còn lại; mất kết nối giữa chừng → trạng thái "Không xác định được trạng thái".

---

## 8. Body ESP32 hai đường (P7.0 mục 8, P7.5 / D13 + Lane 2)

### 8.1 Quyết định tổng thể + điều kiện an toàn Lane 2

- **Hub chấp nhận cả hai đường, một não duy nhất.** Cùng một `Brain` xử lý turn từ cả thiết bị XiaoZhi-OTA và thiết bị protocol v2; transport chỉ là adapter đưa message vào não và đưa action ra body. Không fork não theo đường, không có "não XiaoZhi" riêng. Turn từ ESP32 đi qua cùng guard/approver/audit như turn từ body khác (approver hiển thị trên hub; ESP32 không có màn hình phê duyệt trong v1).
- **Điều kiện an toàn Lane 2 (U1/U2 FAIL) được đưa thành luật thiết kế, không phải ghi chú:** đường XiaoZhi CHỈ được coi là "mã hóa + xác thực" khi TẤT CẢ điều kiện sau cùng đúng (trích đúng kết luận Lane 2 §10): board đường ESP (Wi-Fi/Ethernet/RNDIS/NT26, không dùng board modem EC801E cho đường cần xác thực); OTA server nhà chỉ trả `wss://` + MQTT `:8883` + `https://` (kể cả URL firmware), trừ ngoại lệ lab duy nhất ở §8.2; và chấp nhận audio UDP không xác thực HOẶC ép WebSocket. Bản này quyết ÉP WEBSOCKET (§8.2) nên điều kiện thứ ba được thỏa bằng cách loại bỏ đường MQTT/UDP khỏi v1. Tuyên bố trong docs/UI phải ghi rõ "khi cấu hình như trên", không tuyên bố chung cho firmware.
- **Board modem:** EC801E không xác thực server kể cả khi dùng SSL (`seclevel 0` — Lane 2 §5.2); ML307 chưa xác minh được. Thiết kế này quyết: **hub KHÔNG phân biệt được board modem qua OTA** (OTA JSON không mang board claim đáng tin), nên hub không thể "cấm board modem" bằng code. Biện pháp: (a) docs + UI ghi rõ "chỉ dùng board Wi-Fi/ESP cho đường XiaoZhi nếu cần xác thực; board modem không được tuyên bố xác thực"; (b) OTA response của hub KHÔNG BAO GIỜ chứa firmware URL trong v1 (§8.2) nên kẻ MITM modem không đẩy được firmware qua hub; (c) đường v2 (§8.3) là đường duy nhất được tuyên bố xác thực đầy đủ cho mọi board. Giới hạn này phải nằm trong docs + UI, không giấu (D-DESIGN7-9).

### 8.2 Đường XiaoZhi: OTA nhà + ép WebSocket + lab-only (quyết định cuối, D-DESIGN7-9)

- **OTA server nhà (MỚI, một phần của hub):** endpoint HTTP(S) trả JSON đúng hợp đồng `docs/ota.md` của firmware (đã đọc trong GAP-CHECK): object `websocket { url, token }`, KHÔNG có object `mqtt` (vắng `mqtt` → firmware chọn WebSocket, `application.cc:577-584`), KHÔNG có object `firmware` (không nâng cấp firmware qua OTA trong v1 — xem dưới). OTA URL của firmware phải trỏ về hub nhà (đổi `CONFIG_OTA_URL`, không còn đường gọi về `api.tenclass.net` ở runtime — việc của fork board, §8.5).
- **Luật OTA (một luật duy nhất, gồm ngoại lệ):** mặc định, hàm dựng OTA response chỉ trả URL an toàn — `websocket.url` phải bắt đầu bằng `wss://`, mọi URL firmware (nếu tương lai có) phải `https://`, endpoint MQTT (nếu tương lai có) phải cổng `8883` — và hàm này KHÔNG nhận URL từ cấu hình tự do (URL được dựng từ địa chỉ hub + scheme cố định; cấu hình chỉ cho phép đổi host/cổng, không đổi scheme); ngoại lệ DUY NHẤT là chế độ lab (`lab_mode=true` do user bật rõ trong `[hub.xiaozhi]`, chỉ loopback/tailnet, cảnh báo hiển thị liên tục), trong đó OTA response được trả `ws://` tới địa chỉ loopback/tailnet. Không có ngoại lệ nào khác.
- **Ép WebSocket, không MQTT trong v1:** hub không trả `mqtt` trong OTA JSON, không chạy MQTT broker, không nhận UDP audio. Lý do: audio UDP mã hóa AES-CTR không MAC (Lane 2 §8, GAP-CHECK M2) — chấp nhận nó làm "giới hạn đã biết" trong khi có lựa chọn ép WebSocket là chấp nhận rủi ro không cần thiết. CRITERIA cho phép cả hai lựa chọn; bản này chọn ép WebSocket. MQTT là việc tương lai cần thiết kế riêng (ngoài phạm vi).
- **OTA auth (giới hạn đã biết, nói thật):** firmware XiaoZhi gửi OTA check với header Device-Id (MAC) + Client-Id (UUID), không có chữ ký, không có token đáng tin (UUID/MAC đều giả mạo được trên LAN). Hub OTA endpoint vì vậy: (a) chỉ listen trên LAN/tailnet đã bật (§5.1), không bao giờ internet-facing; (b) allow-list UUID: chỉ UUID đã đăng ký trong UI hub (user nhập UUID hiển thị trên thiết bị) mới nhận được OTA response có `websocket.url` + token; UUID lạ nhận response rỗng/lỗi chung + audit; (c) rate limit: tối đa 1 OTA check mỗi 60 giây mỗi UUID (chống quét); (d) token trong OTA response là token phiên ngắn hạn (hết hạn 1 giờ, dùng 1 lần cho 1 kết nối WS). Token phiên ngắn hạn này chỉ dùng cho 1 kết nối WS XiaoZhi; nó không phải đường xác thực driver đã cắt ở §5.2 và không cấp bất kỳ quyền điều khiển nào. Đây KHÔNG phải xác thực mạnh — docs/UI ghi rõ: "OTA XiaoZhi tin UUID trên LAN; đường này an toàn ở mức LAN tin cậy + tailnet, không phải E2E". Đường v2 mới là đường xác thực mạnh.
- **Không firmware qua OTA trong v1:** OTA response không bao giờ chứa `firmware` object; firmware không bao giờ được nâng cấp qua hub trong Phase 7 (OTA không chữ ký — GAP-CHECK M6; ký số bị cấm ở Phase 7 theo CRITERIA). Nâng cấp firmware là việc thủ công (flash trực tiếp) cho tới khi có OTA ký số ở phase sau. Test khẳng định response không chứa trường firmware với mọi input.
- **WebSocket voice server (MỚI) + phạm vi RFC 6455:** hub chạy WS server nói protocol WS của firmware: hello handshake, audio Opus, message `llm.emotion`, MCP trên cùng kênh (§8.4). Framing WS phải tự triển khai tối thiểu trên TCP+TLS vì cây không có dep WebSocket (probe c5). Phần RFC 6455 được triển khai: bắt tay HTTP Upgrade (Sec-WebSocket-Key/Accept, version 13); đọc frame header (FIN, opcode, mask, độ dài payload gồm dạng 16-bit và 64-bit); bắt buộc masking cho frame từ client; frame dữ liệu text/binary không phân mảnh; ping/pong; bắt tay close; giới hạn kích thước frame (header WS ≤ 16 KiB, payload frame ≤ 256 KiB — vượt thì đóng + audit). Phần bị từ chối — khi gặp, hub đóng kết nối + ghi audit `ws_unsupported_frame` (kèm lý do), không cố hiểu tiếp: frame phân mảnh (FIN = 0 hoặc opcode continuation); mọi extension (bắt tay đề nghị `Sec-WebSocket-Extensions` thì bị từ chối ngay); bit RSV1/RSV2/RSV3 được đặt; frame vượt giới hạn; opcode lạ hoặc opcode dành riêng; frame từ client không có mask; control frame phân mảnh hoặc payload > 125 byte.
- **Cổng bắt buộc ở build cho WS tự triển khai:** test interop với implementation chuẩn (`ws_interop_with_standard_implementation`), test vector frame (`ws_frame_vectors`), và fuzzing (`ws_fuzzing_no_crash_no_bypass`) là cổng bắt buộc — cả ba phải xanh thì đường XiaoZhi mới được coi là xong. Nếu bất kỳ cổng nào thất bại ở build, builder PHẢI leo thang về parent với bằng chứng thất bại; CẤM tự thêm dep tungstenite (hay bất kỳ dep WS nào) âm thầm để lách cổng.
- **TLS cho WS và hệ quả lab-only:** firmware đường ESP validate cert qua Mozilla root bundle (Lane 2 §5.1), không pinning; hub WS trên LAN thường cần cert do CA công khai ký cho hostname của hub, điều không làm được cho LAN IP/hostname trong v1 (không có ACME, không có CA nội bộ trong cây — BỊ CHẶN). Hệ quả đã chốt: trong v1, đường XiaoZhi chỉ chạy được ở chế độ lab (loopback/tailnet, `ws://` plaintext, fake device trên host) cho tới khi có giải pháp cert cho hub; chế độ LAN thường thì OTA chỉ trả `wss://` và hub WS phải có cert hợp lệ. Đây là giới hạn lớn, phải nói rõ trong docs + UI, không hứa quá.
- **Gỡ cổng :8080:** fork board cho Mimint PHẢI gỡ `InitializeWebSocketControlServer()` (GAP-CHECK M9) — mọi điều khiển servo đi qua hub đã auth (§8.4), không có WS local không-auth nào trên thiết bị. Test cổng: grep fork board không còn `websocket_control_server` / `Start(8080)`.

Ghi chú — các phương án đã loại (chỉ ghi ngắn ở đây, không phải thân quyết định): xin dep `tungstenite` (loại theo nguyên tắc 0 dep mới + điều kiện cổng ở trên, D-DESIGN7-8); chấp nhận MQTT/UDP audio không-MAC làm giới hạn đã biết (loại vì ép WebSocket rẻ hơn và an toàn hơn); cert tự ký + fork board tin CA riêng (loại trong v1 vì firmware không có cấu hình CA riêng dễ dàng — Lane 2 §5.1); đầu tư CA nội bộ + fork firmware pin CA trong Phase 7 (loại vì việc lớn, cần phần cứng để kiểm chứng).

### 8.3 Đường protocol v2: spec wire + client tham chiếu + skeleton ESP-IDF

- **Spec wire (MỚI, cùng họ §2):** thiết bị ESP32 đường v2 nói đúng friends handshake §2.4 (HELLO/AUTH/SESSION_CONFIRM) + envelope §2.2, với 2 điều chỉnh cho thiết bị nhúng: (a) transport là TCP + AEAD thuần như §2, không TLS, vì ESP32 chạy AEAD ring-tương đương qua mbedTLS/PSA dễ hơn là chạy TLS + pinning, và §2 đã đủ tính chất; TLS chỉ thêm vào nếu đo cho thấy cần (không đo được trong v1 vì chưa có phần cứng). Quyết định này diễn giải chữ CRITERIA P7.5 "TLS/WSS only, cert pinning" như sau (đã chốt D-DESIGN7-9/D-DESIGN7-2, xem nhật ký độ lệch §14): "TLS/WSS only" áp dụng khi firmware dùng TLS (đường XiaoZhi), còn đường v2 dùng AEAD thuần với pin khóa (tương đương tính chất, không cần X.509). (b) Giới hạn frame nhỏ hơn: header ≤ 8 KiB, payload ≤ 64 KiB (RAM ESP32 hạn chế; audio Opus theo frame nhỏ).
- **Danh tính thiết bị v2:** keypair ECDSA P-256 sinh trên thiết bị lúc provisioning, lưu trong NVS (v1: NVS plaintext như firmware hiện tại — giới hạn đã biết M5; NVS encryption là việc tương lai §13). Pairing thiết bị v2 dùng luồng gõ tay của ceremony §3.3 (thiết bị hiển thị SAS/fingerprint qua màn hình của nó nếu có, hoặc qua serial/log trong lab — v1 chỉ yêu cầu fake device trên host, xem dưới). Thu hồi/xoay key ở hub như §3.6.
- **Client tham chiếu chạy trên host (Rust, MỚI):** chương trình Rust (trong cây, dạng example/bin test) triển khai đủ handshake §2.4 + trao đổi message mã hóa với hub, chứng minh protocol v2 chạy được đầu-cuối mà không cần phần cứng. Client này là bằng chứng G2 cho đường v2 (cùng với fake device XiaoZhi).
- **Firmware skeleton ESP-IDF (MỚI):** thư mục firmware riêng (ngoài crate Rust) với cấu hình transport (chỉ nối tới hub đã pin, không plaintext tới host lạ, cert pinning nếu dùng TLS), cấu trúc task (Wi-Fi → handshake → audio loop → MCP servo), và README cài đặt toolchain chính xác. **Biên dịch được nếu toolchain có sẵn trên vega, nếu không thì ghi BỊ CHẶN kèm bước cài đặt chính xác, không tuyên bố đã biên dịch** (CRITERIA P7.5). Giai đoạn 2 phải kiểm tra toolchain trên vega trước khi tuyên bố gì về build firmware. Skeleton không cần chạy được trên phần cứng trong Phase 7 (phần cứng BỊ CHẶN).

### 8.4 MCP thiết bị + servo

- **MCP trên kênh voice (đường XiaoZhi):** firmware chạy MCP server JSON-RPC 2.0 trên cùng kênh WS (GAP-CHECK §2.1); hub là MCP client gửi `tools/list`/`tools/call` cho lệnh servo. Hub KHÔNG tin `user_only`: mọi tool (kể cả tool user-only như reboot/upgrade) đều qua allow-list của hub — tool nào không có trong allow-list servo của hub thì hub không bao giờ gọi, kể cả khi firmware liệt kê nó. Đặc biệt: `reboot`/`upgrade_firmware` KHÔNG BAO GIỜ được hub gọi trong Phase 7 (không có đường code nào gọi chúng; test cổng grep khẳng định).
- **Allow-list servo (hub-side, theo vai trò):** chỉ các lệnh di chuyển an toàn trong allow-list mới được gọi: di chuyển theo mẫu có sẵn (walk/turn/sit/stand/stop...), `stop` luôn được phép, `servo_move` góc tuyệt đối chỉ khi góc trong giới hạn clamp của board (hub kiểm tra lại giới hạn, không tin clamp của firmware), `servo_sequences` (JSON sequence do AI lập trình) CHỈ được phép khi sequence đã qua phê duyệt của user (thẻ phê duyệt hiện nội dung sequence) — sequence tự do là code chạy trên robot, phải qua approver như `execute_code`. Lệnh trim/status/battery: đọc, được phép.
- **An toàn servo (luật của hub, không tin firmware):** mọi lệnh di chuyển có timeout (mặc định 30 giây, như timeout action của actor); lệnh `stop` có đường ưu tiên (gửi ngay, không xếp hàng sau lệnh đang chạy — hub giữ 1 kênh stop riêng/khẩn cấp); mất kết nối → hub coi robot là "trạng thái chưa biết" (như `UnknownOutcome`), không gửi tiếp lệnh khi nối lại mà không xác minh trạng thái; không có lệnh nào được retry tự động sau mất kết nối (tránh robot di chuyển 2 lần).
- **Chứng minh bằng fake device trên host:** fake device ESP32 (Rust, chạy trên vega) giả lập cả hai đường (WS XiaoZhi + protocol v2), nhận lệnh MCP servo, ghi lại lệnh đã nhận, trả telemetry giả. Test G2 chạy fake device qua cả hai đường nối hub, assert lệnh servo tới đúng + allow-list chặn đúng. Không có phần cứng thật — mọi tuyên bố về robot thật đều cấm (§8.5).

### 8.5 Bảng mapping message WS XiaoZhi → xử lý của hub (đường XiaoZhi)

Bảng này liệt kê message WS mà firmware gửi/nhận theo hợp đồng đã đọc trong GAP-CHECK, và cách hub xử lý từng loại trong v1. Message nào không có trong bảng thì hub bỏ qua + audit (không crash, không hiểu nhầm thành lệnh).

| Message (firmware ↔ hub) | Hướng | Hub xử lý trong v1 |
|---|---|---|
| `hello` (device id, version, audio params) | Thiết bị → hub | Kiểm tra UUID trong allow-list + token phiên (§8.2); từ chối nếu không khớp; trả hello ack với cấu hình audio (Opus 16 kHz mono). |
| Audio Opus (binary frame) | Thiết bị → hub | Đưa vào pipeline ASR của não (ASR là provider theo D9; hub không tự nhận dạng trong v1 nếu chưa có ASR — ghi rõ: voice đầy đủ cần ASR/TTS, engine voice bị cấm ở Phase 7 nên audio trong v1 chỉ được ghi nhận + trả về emotion/ack, chưa thành turn hoàn chỉnh bằng giọng nói. Xem lưu ý dưới bảng.) |
| Audio TTS (binary frame) | Hub → thiết bị | Chỉ khi có TTS (D9, ngoài Phase 7): hub gửi Opus trả về. Trong v1 không có TTS thì không gửi audio trả về. |
| `llm.emotion` | Hub → thiết bị | Hub gửi emotion theo trạng thái turn (map 9 trạng thái của mặt Mimint sang tên emotion firmware hiểu — mapping là cấu hình, không hardcode tên andee). |
| `mcp` (JSON-RPC tools/list + tools/call) | Hub → thiết bị, thiết bị trả kết quả | Hub là client MCP (§8.4): chỉ gọi tool trong allow-list servo; `tools/list` chỉ để khám phá, không tin mù. |
| `system` (reboot/upgrade) | Hub → thiết bị | KHÔNG BAO GIỜ gửi trong Phase 7 (xem §8.4). Thiết bị gửi `system` → hub bỏ qua + audit. |
| `abort` / `listen` state (detect/stop) | Cả hai | Hub cập nhật trạng thái phiên voice; không có tác dụng phụ lên body. |
| Message lạ / JSON sai | Thiết bị → hub | Bỏ qua + audit `unknown_ws_message`; không đóng kết nối vì 1 message lạ (trừ khi message lạ lặp lại > 10 lần/phút → đóng + audit, chống spam). |

**Lưu ý trung thực về voice đường XiaoZhi trong Phase 7:** CRITERIA cấm "engine voice" ở Phase 7. Vì vậy đường XiaoZhi trong Phase 7 chứng minh được: kết nối WS + hello + OTA + MCP servo + emotion — nhưng KHÔNG chứng minh được turn voice hoàn chỉnh (nghe → hiểu → trả lời bằng giọng nói), vì ASR/TTS là engine voice của phase khác (D9). Fake device test (§8.7) gửi audio frame giả để chứng minh transport, không chứng minh ASR/TTS. Docs + UI phải ghi rõ ranh giới này; không tuyên bố "robot nghe-nói được" từ Phase 7.

**Nhãn trạng thái ESP32 tách bạch (bắt buộc, §12.3 hiển thị đúng các nhãn này):** UI dùng đúng 4 nhãn tách bạch theo đường/chế độ, không gộp: "Lab mode — không mã hóa" (chế độ lab §8.2); "XiaoZhi LAN — xác thực giới hạn" (đường XiaoZhi, tin UUID trên LAN); "Firmware v2 — khóa đã pair" (đường protocol v2, xác thực bằng khóa đã pair); "Voice hoàn chỉnh — chưa hỗ trợ Phase 7" (ghi rõ trên mọi màn hình voice ESP32). CẤM nhãn tổng thể "Robot đã kết nối an toàn" hay bất kỳ nhãn chung nào gộp các mức an toàn khác nhau thành một trạng thái.

### 8.6 BỊ CHẶN giữ nguyên (đường ESP32)

Chạy thật trên phần cứng ESP32 (chưa chốt phần cứng, chưa có khung chó/servo); bắt tay TLS thật trên modem; build firmware nếu toolchain vắng (ghi BỊ CHẶN + bước cài); wake word tiếng Việt trên chip (U6); `servo_dog_ctrl` (U5); quét port local toàn cây board (U8). Fork board ứng viên (khi có phần cứng): `eda-robot-pro` (ESP32-S3, 4 servo PWM, code hoàn toàn trong cây, MCP chó đầy đủ — GAP-CHECK C28) là mẫu tốt nhất; `esp-hi` (ESP32-C3, chó thật) phụ thuộc component ngoài chưa kiểm tra. Quyết phần cứng vẫn chờ user.

### 8.7 Kiểm chứng (ESP32)

- `ota_only_safe_urls`: hàm dựng OTA response khẳng định đúng luật mới ở §8.2: mặc định (lab tắt), với mọi cấu hình lệch (kể cả scheme độc hại trong cấu hình) → output chỉ chứa `wss://` / `https://` / MQTT cổng 8883, không bao giờ chứa `ws://`, `http://`, MQTT cổng khác, hay object `firmware`; riêng ca lab (`lab_mode=true`) → output được chứa `ws://` tới địa chỉ loopback/tailnet và chỉ trong ca đó (test riêng cho ca lab, không gộp vào khẳng định mặc định).
- `ota_forces_websocket`: OTA response không chứa object `mqtt` (mọi trường hợp v1).
- `ota_unknown_uuid_rejected`: UUID lạ → response rỗng + audit; UUID đã đăng ký → response có URL + token.
- `ws_interop_with_standard_implementation`: WS server của hub trao đổi frame đúng với một implementation WebSocket chuẩn độc lập (client chuẩn nối tới, bắt tay + gửi/nhận frame text/binary/ping/pong/close đều đúng); đây là cổng bắt buộc ở build (§8.2).
- `ws_frame_vectors`: test vector frame cố định — frame header chuẩn, frame có mask, frame độ dài 16-bit/64-bit, frame phân mảnh, frame RSV, opcode lạ — mỗi vector khẳng định đúng hành vi chấp nhận/từ chối + đóng + audit theo bảng §8.2.
- `ws_fuzzing_no_crash_no_bypass`: fuzzing frame WS ngẫu nhiên + biến dạng trong thời lượng cố định → không crash, không panic, không frame độc hại nào được hiểu thành message hợp lệ hay vượt qua giới hạn; mọi input lạ đều đóng + audit. Cổng bắt buộc ở build (§8.2).
- `xiaozhi_fake_device_end_to_end`: fake device WS nối hub (chế độ lab), hello + audio frame + emotion + MCP servo call → hub xử lý đúng, 1 não duy nhất (cùng Brain xử lý turn từ fake device và từ body desktop — test assert cùng code path).
- `v2_reference_client_handshake`: client tham chiếu Rust bắt tay + trao đổi message mã hóa với hub; peer lạ bị từ chối như §2.6.
- `mcp_servo_allowlist_enforced`: hub không bao giờ gọi `reboot`/`upgrade_firmware` (grep cổng + test gọi thử bị chặn ở hub); `servo_sequences` không qua approver → bị chặn; góc servo ngoài clamp → bị chặn ở hub.
- `mcp_user_only_not_trusted`: firmware liệt kê tool user-only → hub vẫn chặn gọi nếu tool không trong allow-list.
- `esp32_no_8080_in_fork`: grep fork board (khi có) không còn WS control server :8080.
- `esp32_firmware_build_blocked_honest`: nếu toolchain vắng trên vega, test/tài liệu ghi BỊ CHẶN + bước cài đặt chính xác; không test nào tuyên bố đã biên dịch firmware khi chưa biên dịch.

---

## 9. Privacy Mode (P7.0 mục 9) — quyết định + ranh giới

### 9.1 QUYẾT ĐỊNH: sync LAN tới peer đã pair VẪN CHẠY khi Privacy Mode BẬT

> **Privacy Mode BẬT không chặn sync LAN tới peer đã pair (owner).** Sync là trao đổi trực tiếp, mã hóa E2E, giữa hai máy của cùng một user, không qua server, không qua cloud, không qua relay. Privacy Mode chặn EGRESS (dữ liệu rời khỏi máy user tới bên thứ ba) — sync LAN không phải egress theo nghĩa đó.

**Văn bản Privacy Mode hiển thị cho user (bắt buộc, §12.3):** màn hình Privacy hiển thị đúng nội dung sau bằng chữ, kèm liên kết tới danh sách peer; CẤM chỉ dùng biểu tượng khiên (xanh hay bất kỳ màu nào) để thông báo Privacy Mode đang bật:

"Privacy Mode đang bật. Provider egress bị giới hạn: dữ liệu chỉ tới backend trên máy này. Các kết nối sau VẪN được phép và vẫn truyền dữ liệu: đồng bộ dữ liệu với thiết bị đã ghép đôi; phê duyệt yêu cầu từ peer đã được cấp quyền; bàn giao nhiệm vụ giữa các thiết bị đã ghép đôi. Lưu ý: Privacy Mode không ngăn truyền dữ liệu qua kết nối trực tiếp tới thiết bị đã ghép đôi — dữ liệu sync, phê duyệt và handoff vẫn chạy qua LAN/tailnet tới peer đã pair. Hãy kiểm tra danh sách thiết bị đáng tin cậy. [Xem danh sách thiết bị đã ghép đôi]"

Lý do (theo thứ tự):

1. **Định nghĩa Privacy Mode trong cây là egress kill-switch ở tầng HTTP provider** (`egress.rs`: chỉ loopback backend khi ON). Sync friends không phải HTTP, không phải provider, không đi qua `GuardedHttpClient`. Ép Privacy Mode chặn sync là mở rộng định nghĩa mà không có cơ sở trong code hiện có — và sẽ phá vỡ lời hứa D6 ("đồng bộ E2E ngay từ v1") trong chính chế độ mặc định (Privacy ON là mặc định — PROPOSALS Q5). User bật Privacy Mode (mặc định) mà sync không chạy thì Phase 7 vô nghĩa trong cấu hình mặc định.
2. **Peer đã pair là "máy của user", không phải bên thứ ba.** Pairing ceremony (§3) là hành động tin cậy tường minh của user. Dữ liệu sync tới peer đã pair giống như dữ liệu user tự copy USB sang máy kia — có chủ đích, có kiểm soát, mã hóa trên đường đi.
3. **Sync không bao giờ mang thứ Privacy Mode bảo vệ khỏi cloud:** không secret, không history, không audit (§4.2). Thứ sync mang (SOUL, skill đã promote, note shared, profile) là thứ user đã chọn chia sẻ giữa các máy của mình.
4. **Tiền lệ trong cây:** notebook là local-only với 1 điểm lọc `forHub()` — ranh giới là "cái gì rời khỏi máy tới HUB/cloud", không phải "cái gì rời khỏi máy tới máy khác của user". Sync peer-to-peer nằm cùng phía với "máy của user" như hub local.

Phương án đã loại: (a) **Privacy ON chặn sync** — loại vì lý do 1 (phá D6 trong cấu hình mặc định) và vì user sẽ tắt Privacy Mode chỉ để sync chạy, làm mất bảo vệ egress thật. (b) **Privacy ON chỉ cho sync loopback** — loại vì vô nghĩa thực tế (2 body khác nhau hiếm khi cùng loopback, trừ test) và vì ranh giới thật là "đã pair hay chưa", không phải "loopback hay LAN". (c) **Thêm công tắc sync riêng, mặc định tắt khi Privacy ON** — loại vì thêm công tắc gây nhầm lẫn; công tắc đúng là selective sync theo kind (§4.7) + bật/tắt theo peer, độc lập với Privacy Mode.

### 9.2 Ranh giới ngăn sync rò ra cloud/relay

Dù sync chạy khi Privacy ON, các ranh giới sau là CỨNG và được ép bằng code:

1. **Địa chỉ peer phải là địa chỉ private/loopback/link-local.** Trước khi nối, hub/client kiểm tra địa chỉ đích: chỉ chấp nhận `127.0.0.0/8`, `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `::1`, `fe80::/10`, `fc00::/7` (ULA). Tailnet CGNAT `100.64.0.0/10` cũng ĐƯỢC chấp nhận vì user đã chốt "chủ yếu LAN + tailnet" — PROPOSALS Q2; địa chỉ này không route ra internet công cộng. Địa chỉ public khác → từ chối nối + audit. Không có ngoại lệ cấu hình trong v1 (D-DESIGN7-10).
2. **Không tin hostname mù.** Nếu user nhập hostname thay vì IP, resolver phải resolve và kiểm tra TẤT CẢ địa chỉ trả về đều nằm trong dải cho phép (kỷ luật `LoopbackGuardResolver` tổng quát hóa); bất kỳ địa chỉ nào ngoài dải → từ chối toàn bộ. Không có DNS rebinding nào đưa sync ra internet được.
3. **Không có code relay/cloud nào trong cây.** Không có client nào nối tới server trung gian, không có telemetry, không có "phone home". Test cổng quét: code sync/hub không chứa URL/hostname ngoài nào, không gọi `GuardedHttpClient` với non-loopback khi Privacy ON.
4. **OTA XiaoZhi cũng chịu ranh giới địa chỉ:** OTA response chỉ chứa URL trỏ về địa chỉ hub trong dải private/loopback; hub không trả URL public.
5. **Guest không sync** (§3.5) — ranh giới trust, độc lập với ranh giới địa chỉ.

### 9.3 Tương tác với Privacy Mode ở các chỗ khác

- **Phê duyệt từ peer:** chạy khi Privacy ON (cùng lý do §9.1 — peer đã pair, LAN, E2E). Thẻ phê duyệt mang tool + args (có thể chứa dữ liệu nhạy cảm của lượt) — nhưng chỉ tới peer owner đã pair, qua kênh mã hóa. User không muốn thì tắt opt-in theo peer (§6.2).
- **Handoff:** chạy khi Privacy ON (cùng lý do). Gói handoff không mang secret (§7.1).
- **Hub LAN listen:** độc lập với Privacy Mode (bật LAN là quyết định riêng §5.1). Privacy ON không tự bật/tắt LAN.
- **Backend LLM:** không đổi. Privacy ON vẫn chỉ cho loopback backend như hôm nay; sync không ảnh hưởng gì tới egress provider.

### 9.4 Kiểm chứng (Privacy Mode)

- `privacy_on_sync_still_runs`: Privacy ON (mặc định) + 2 instance đã pair → sync chạy bình thường, record tới đủ.
- `privacy_on_sync_refuses_public_address`: peer nhập địa chỉ public (ví dụ `8.8.8.8`) → từ chối nối trước khi mở socket, audit ghi rõ.
- `privacy_on_sync_refuses_dns_rebinding`: hostname resolve ra cả private + public → từ chối toàn bộ.
- `privacy_on_no_relay_code`: test cổng quét — code Phase 7 không chứa hostname/URL relay/cloud nào; không có kết nối nào ra ngoài dải private trong test sync đầy đủ (bắt socket trong harness).
- `privacy_mode_backend_unchanged`: suite egress cũ pass nguyên vẹn (không sửa ngữ nghĩa `EgressPolicy`).

---

## 10. Hiệu năng máy yếu (P7.0 mục 10) — ngân sách viết trước, đo thật ở G2

### 10.1 Số đo đã có (probe §0.3, release, vega i5-2520M)

| Thao tác | Số đo thật | Ghi chú |
|---|---|---|
| Sinh 1 keypair P-256 | 24 µs | Trung bình 200 lần |
| 1 cặp ECDH + HKDF (2 keygen + 2 agree + 2 HKDF) | 445 µs | Trung bình 200 cặp — đây là phần crypto nặng nhất của handshake |
| AES-256-GCM seal 1 KiB | 1 µs | Trung bình 1000 lần |
| Toàn bộ handshake crypto (ước từ probe) | < 1 ms | Chưa đo đầu-cuối (cần code + mạng); RTT LAN + IO sẽ chiếm phần lớn thời gian thật |

Kết luận: **crypto không phải nút thắt.** Ngân sách dưới đây đặt theo RTT + IO + xử lý record, không theo crypto.

### 10.2 Ngân sách mục tiêu (viết trước, đo thật ở cổng G2 trên vega)

| # | Thao tác | Mục tiêu (vega, LAN loopback) | Cách đo ở G2 |
|---|---|---|---|
| B1 | Handshake friends đầy đủ (TCP connect → SESSION_CONFIRM xong) | < 500 ms (p95, 20 lần) | Harness 2 instance, đo từ connect tới confirm, ghi phân phối |
| B2 | Pairing ceremony (từ pair_request tới pair_accept, không tính thời gian user gõ/so sánh) — đo cho CẢ HAI luồng: luồng so sánh mặc định và luồng gõ tay fallback | < 2 giây (phần máy, mỗi luồng) | Harness mô phỏng user trả lời ngay; thời gian user không tính; hai luồng đo riêng, cả hai đều phải đạt |
| B3 | Sync 100 record nhỏ (mỗi ≤ 4 KiB) 2 chiều | < 2 giây (toàn phiên) | 2 instance, record tạo sẵn, đo từ sync_offer tới sync_ack cuối |
| B4 | Sync 1 record SOUL 64 KiB | < 500 ms | Như B3, 1 record lớn |
| B5 | Xử lý 1 message đã mã hóa (giải mã + dispatch, không tính IO) | < 5 ms | Đo trong tiến trình bằng Instant, 1000 message |
| B6 | Khởi động hub (từ lệnh bật tới listen xong) | < 1 giây | Đo từ lúc gọi start tới khi accept được kết nối đầu tiên |
| B7 | Bộ nhớ tăng thêm khi hub + 4 session mở | < 50 MiB so với baseline không hub | Đo RSS trước/sau (cùng phương pháp đo RSS Phase 0 Lane 3) |
| B8 | CPU khi idle (hub mở, không session) | ~0% (không polling busy) | Đo CPU 60 giây idle; polling sleep phải ≥ 50 ms nếu có |

### 10.3 Luật hiệu năng (không hứa quá)

- Không có mục tiêu nào cho MacBookPro11,1 trong bản này (chưa xác nhận lại cấu hình — CRITERIA ghi rõ). Đo trên Mac là việc của G2 nếu máy sẵn sàng; không tuyên bố số Mac.
- Không có mục tiêu cho ESP32 (chưa có phần cứng, chưa đo được gì trên chip). Mọi số ESP32 đều BỊ CHẶN.
- Nếu đo thật ở G2 vượt ngân sách: ghi rõ số thật + nguyên nhân vào docs, không sửa ngân sách cho vừa số đo mà không qua review. Ngân sách là mục tiêu, không phải lời hứa đã đạt.
- Giới hạn đã biết: sync record lớn (tới 256 KiB) + batch 4 MiB trên máy yếu có thể gây giật UI nếu chạy trên UI thread — sync PHẢI chạy trên worker thread/task riêng, không bao giờ trên UI frame path (cùng kỷ luật Keychain §0.4). UI chỉ nhận tiến trình qua channel.

---

## 11. Đối chiếu cổng (P7.1–P7.6 → § thiết kế → cổng G1–G4)

### 11.1 Bảng đối chiếu

| Yêu cầu CRITERIA | § thiết kế | Cổng kiểm chứng | Test nêu tên (tóm tắt — đầy đủ ở từng §) |
|---|---|---|---|
| P7.1 Danh tính + pairing (keypair, registry, SAS, revoke, rotate, guest) | §3 | G1 (test xanh), G2 (2 instance pair thật) | §3.8 (17 test) |
| P7.2 Sync E2E (ECDH+AEAD, tập sync, versioning/conflict, tombstone, nhập tay) | §2 + §4 | G1, G2 (sync 2 chiều + conflict UI), G3 (quét âm secret) | §2.6 (14 test) + §4.8 (19 test) |
| P7.3 Hub auth + mã hóa (loopback, xác thực lẫn nhau bằng khóa đã pin, allow-list, đèn + ngắt, cấm endpoint không-auth) | §5 | G1, G2 (fake device + hub), G3 (quét endpoint) | §5.5 (10 test) |
| P7.4 Phê duyệt xuyên thiết bị (binding, 60s, audit device, hết hạn/batch) | §6 | G1, G2 (approve từ peer) | §6.4 (11 test) |
| P7.5 ESP32 hai đường (1 não, OTA an toàn, v2 spec + client + skeleton, MCP servo, BỊ CHẶN) | §8 | G1, G2 (fake device 2 đường), G4 (BỊ CHẶN ghi rõ) | §8.7 (12 test) |
| P7.6 Handoff (gói, lease 1 actuator, xác minh, test 2 instance) | §7 | G1, G2 (handoff thật) | §7.4 (11 test) |
| P7.0-1 Threat model | §1 | G4 (tài liệu) | §1.4 (1 test) |
| P7.0-9 Privacy Mode | §9 | G1 (test ranh giới), G3 | §9.4 (5 test) |
| P7.0-10 Hiệu năng máy yếu | §10 | G2 (số đo thật trên vega) | B1–B8 (§10.2) |
| G1 Clone mới, test xanh + clippy + fmt | — (cổng build) | G1 | Toàn bộ 100 test mới nêu tên ở §1.4/§2.6/§3.8/§4.8/§5.5/§6.4/§7.4/§8.7/§9.4 + 662 test cũ, 0 fail; `cargo clippy -D warnings`; `cargo fmt --check`; baseline single-agent không đổi hành vi |
| G3 License + cây sạch | §0.2 (0 dep mới), §4.2, §5.2 | G3 | License audit 0 forbidden; README + assets byte-identical; quét secret 0 trúng; quét âm endpoint + payload sync |
| G4 Bằng chứng + BỊ CHẶN | §13, Phụ lục | G4 | Bằng chứng vào `phase7/`; mọi mục BỊ CHẶN vẫn ghi BỊ CHẶN |

### 11.2 Quy tắc test Phase 7 (kế thừa các phase trước)

- Test 2 instance thật chạy trên vega với 2 data dir riêng + 2 cổng riêng, qua loopback (đại diện cho LAN ở mức protocol; LAN thật qua 2 máy là việc của G2 nếu có 2 máy — không tuyên bố đã test LAN 2 máy khi chỉ test loopback).
- Test crypto dùng key thật sinh bằng ring (không dùng key giả cố định, trừ test vector có chủ đích như test vector suy ra khóa ở §2.4 và RFC 4231 của Android protocol). Test sai mã/khóa sai/tamper phải assert FAIL, không assert "không crash".
- Test không được sửa ngữ nghĩa test cũ (G1). Test cũ nào cần cập nhật do migration `sync_id` thì chỉ cập nhật fixture/schema, không đổi assertion hành vi.
- Mọi test mới đều có tên nêu rõ tính chất nó khóa (theo mẫu Phase 6: `g6_1_...`, ở đây tiền tố theo §: `friends_...`, `pairing_...`, `sync_...`, `hub_...`, `approve_...`, `handoff_...`, `ota_...`, `ws_...`, `mcp_...`, `esp32_...`, `privacy_...`, `threat_model_...`).

---

## 12. Bề mặt cấu hình + UI (phác thảo — giai đoạn 2 mới viết code parse)

### 12.1 Cấu hình mới (TOML, `deny_unknown_fields`, theo mẫu `FileConfig`)

```toml
# Mặc định (không có section nào): hub loopback-only, không LAN, không tự sync,
# không phê duyệt từ peer. Mọi tính năng Phase 7 đều tắt an toàn cho tới khi bật rõ.

[friends]
# Tên hiển thị của body này khi pair (mặc định: hostname đã sanitize).
device_name = "vega"
# Tự động sync khi bắt tay thành công với peer owner (mặc định: false).
auto_sync_on_connect = false

[hub]
# Lắng nghe trên LAN (mặc định: false = chỉ loopback). Bật LAN yêu cầu pairing (§5.1).
listen_lan = false
# Cổng friends (mặc định: 19120). Cổng OTA/WS XiaoZhi cấu hình riêng dưới đây.
port = 19120
# Tối đa session friends đồng thời (mặc định: 4, khoảng 1..8).
max_sessions = 4

[hub.xiaozhi]
# Bật OTA + WS voice cho đường XiaoZhi (mặc định: false).
enabled = false
# Chế độ lab (loopback/tailnet, WS plaintext) — xem §8.2 (D-DESIGN7-9). Mặc định: false.
lab_mode = false
# Cổng WS voice (mặc định: 19121). Cổng OTA HTTP (mặc định: 19122).
ws_port = 19121
ota_port = 19122

[sync]
# Bật/tắt theo kind (mặc định: tất cả true cho owner). Tắt kind nào thì kind đó
# không sync chiều nào (§4.7). Không có công tắc nào bật được kind bị cấm (§4.2).
soul_files = true
skills = true
notebook_shared = true
todos_plans = true
naming_profiles = true
```

- Mọi section mới đều `#[serde(deny_unknown_fields)]` + validate fail-closed như `ProfileSet::resolve`: giá trị ngoài khoảng → lỗi cấu hình rõ ràng, không dùng giá trị mặc định âm thầm thay cho giá trị sai.
- Không có secret nào trong file cấu hình. Khóa thiết bị không nằm trong cấu hình.
- Cấu hình thay đổi chỉ có hiệu lực sau khi rebuild/restart đường liên quan (như `EgressPolicy` cố định lúc build client — không có setter chạy nóng cho bind address hay trust).

### 12.2 File dữ liệu mới trong data dir

| File | Nội dung | Sync? | Ghi chú |
|---|---|---|---|
| `device-identity.key` | Private key ECDSA P-256 (PKCS#8 DER), 0600 | KHÔNG BAO GIỜ | §3.2. Mất = pair lại từ đầu. |
| `peers.db` (SQLite) | Registry thiết bị, pairing attempts/lockout, sync state, sync conflicts, tombstones | KHÔNG | §3.7, §4.4–§4.5, §4.7. |
| `notes.db` (hiện có) | Thêm cột `sync_id` cho `notes` + bảng `todos`/`plans` mới | Nội dung record thì sync; DB file thì không | §4.3, §4.6. Migration idempotent. |

### 12.3 UI yêu cầu (tóm tắt — chi tiết ở từng §)

- Quản lý thiết bị (§3.7): danh sách, thu hồi/xóa/đổi tên/đổi trust (phân biệt thu hồi/xóa theo §3.6, bố trí không gây nhầm), nút "Đặt lại danh tính bảo mật của thiết bị này" kèm cảnh báo, bật chế độ pairing, trạng thái lockout, văn bản cảnh báo mất khóa §3.2, nhãn trust theo hướng, trạng thái "Pairing chưa hoàn tất".
- Pairing (§3.3–§3.4): màn hình hiển thị SAS hai bên (luồng chính) hoặc ô gõ SAS (luồng fallback), fingerprint rút gọn có mở rộng + nút sao chép + nhóm 4, xác nhận đối chiếu fingerprint riêng từng phía, đếm ngược 5 phút, nút hủy ceremony, hướng dẫn bắt đầu lại, thông báo theo từng ca (hết hạn, sai mã, SAS lệch, lockout kèm thời điểm thử lại, không ở chế độ pairing), chọn trust theo hướng từng phía.
- Sync (§4): nút "Đồng bộ ngay" theo peer, tiến trình + kết quả phiên, màn hình xung đột (diff + 4 lựa chọn + giải thích hậu quả + quy tắc giữ cả hai + câu giải thích vô hiệu cho SOUL/profile + tuyên bố không hoàn tác), thông báo tombstone (nhãn nút cụ thể + tuyên bố dữ liệu vẫn còn + giới hạn 90 ngày + resurrection), công tắc selective sync hai cấp + câu cố định về notebook Shared.
- Hub (§5): 5 trạng thái phơi nhiễm (§5.1), nút đóng LAN ngay kèm xác nhận liệt kê session + tuyên bố đóng LAN ≠ thu hồi pairing, đèn báo driver từ xa + nút ngắt ngay.
- Phê duyệt (§6): thẻ từ peer đủ trường nguồn gốc (§6.2) + dòng nguồn thiết bị; công tắc theo peer "cho phép trả lời thẻ".
- Handoff (§7): nút gửi handoff, màn hình xem gói + Tiếp tục/Từ chối, thanh trạng thái cố định 7 trạng thái + người giữ quyền + thời gian còn lại.
- ESP32 (§8): 4 nhãn trạng thái tách bạch (§8.5), cảnh báo giới hạn Lane 2 (hiển thị đúng mức an toàn của cấu hình hiện tại, không tuyên bố quá), danh sách UUID đã đăng ký, trạng thái fake device (lab).
- Privacy (§9): văn bản Privacy Mode đầy đủ §9.1 (bảo vệ cái gì + không bảo vệ cái gì + liên kết tới danh sách peer), hiển thị trong màn hình Privacy hiện có, không tạo màn hình riêng; cấm chỉ dùng biểu tượng khiên.
- CLI: mọi thông tin trên in được dưới dạng text (pair/sync/hub/approve/handoff đều có lệnh CLI tương đương cho test G2 trên vega không có GUI), dùng cùng từ ngữ với GUI theo bảng §12.4.

### 12.4 Nhất quán ngôn ngữ CLI/GUI (bắt buộc)

CLI và GUI dùng cùng một từ cho cùng một hành động; bảng dưới đây là chuẩn duy nhất. CLI không buộc user hiểu cấu trúc dữ liệu nội bộ (không lộ tên bảng, tên cột, tên kind wire trong thông báo user thấy).

| Hành động | Từ dùng chung CLI + GUI | Không dùng |
|---|---|---|
| Thu hồi thiết bị | Thu hồi (revoke) | Xóa, gỡ, chặn |
| Xóa entry khỏi danh sách | Xóa khỏi danh sách (delete) | Thu hồi, gỡ |
| Xoay key của chính mình | Đặt lại danh tính bảo mật của thiết bị này | Xoay key, reset key |
| Hạ/nâng trust | Hạ xuống Guest / Pair lại để nâng lên Owner | Đổi quyền (chung chung) |
| Gửi handoff | Gửi bàn giao (handoff offer) | Chuyển, gửi task |
| Chấp nhận handoff | Tiếp tục ở đây (handoff accept) | Đồng ý, OK |
| Đồng bộ | Đồng bộ ngay (sync) | Cập nhật, kéo |
| Giải quyết xung đột | Giải quyết xung đột (resolve conflict) | Đồng bộ, hợp nhất |
| Bật/tắt Privacy Mode | Bật/tắt Privacy Mode | Bật/tắt đồng bộ |
| Đóng LAN | Đóng LAN ngay | Ngắt kết nối, thu hồi |

---

## 13. Mục còn mở / BỊ CHẶN — thiết kế này KHÔNG quyết thay

### 13.1 BỊ CHẶN (giữ nguyên, không tuyên bố đã xong)

- Chạy thật trên phần cứng ESP32; bắt tay TLS thật trên modem; build firmware ESP-IDF nếu toolchain vắng trên vega (giai đoạn 2 kiểm tra + ghi BỊ CHẶN kèm bước cài đặt chính xác nếu vắng).
- Chạy thật trên Mac/Windows/Android (pairing/sync/hub trên các OS đó chưa kiểm chứng; thiết kế viết cho mọi OS nhưng chỉ chứng minh trên Linux/vega).
- Transport zbus Wayland; ký số (firmware + app); huấn luyện thật; engine voice/proactive/scheduler; Best-of-N (Phase 8); grounder D12.
- Wake word tiếng Việt trên chip (U6); `servo_dog_ctrl` (U5); quét port local toàn cây board (U8); ngữ nghĩa `MSSLCFG` của ML307.
- Cert hợp lệ cho hub WS trên LAN (ACME/CA nội bộ) — xem §8.2 (D-DESIGN7-9).
- Keyring ghi khóa thiết bị (macOS Keychain ghi, Windows Credential Manager, Linux Secret Service) — xem §3.2 (D-DESIGN7-3).

### 13.2 mDNS discovery — thiết kế tương lai (KHÔNG triển khai ở Phase 7)

CRITERIA yêu cầu chỉ ghi thiết kế tương lai cho mDNS, v1 dùng nhập địa chỉ tay. Thiết kế tương lai (chưa quyết, chưa triển khai, cần một vòng thiết kế riêng sau Phase 7):

- Service type riêng của Mimint (tên mới, KHÔNG dùng tên service của andee), quảng bá: device_id + tên hiển thị + cổng hub + fingerprint rút gọn. Discovery CHỈ tìm địa chỉ — không tự pair, không tự tin: tìm thấy peer qua mDNS vẫn phải qua ceremony §3 đầy đủ (SAS + fingerprint) mới được tin. Quảng bá có công tắc tắt (mặc định TẮT — không quảng bá danh tính ra LAN khi user không muốn). mDNS không bao giờ thay thế kiểm tra pin (§2.4) hay ranh giới địa chỉ (§9.2). Đây là phác thảo 1 đoạn, không phải spec; triển khai mDNS cần thiết kế + vòng soát xét riêng.

### 13.3 Cải tiến tương lai (không phải Phase 7)

- QR pairing (mang fingerprint, quét để đối chiếu nhanh — vẫn cần SAS ngoài băng, QR không thay SAS).
- Keyring ghi khóa thiết bị theo OS (D-DESIGN7-3).
- Relay tự host E2E-mù (P1, A P1-3) — chỉ sau khi LAN/tailnet đã vững; relay không bao giờ thấy plaintext.
- Merge 3-way có giám sát cho conflict (thay vì chỉ chọn 1 trong 4).
- MQTT/DTLS cho đường XiaoZhi; NVS encryption + secure boot cho firmware v2; OTA ký số (P0-6).
- Giới thiệu peer (trust transitivity) — cần thiết kế trust riêng, không có trong v1.

---

## 14. QUYẾT ĐỊNH ĐÃ CHỐT + SIGN-OFF (vòng review thiết kế, 19/11/2026)

Mọi điểm J1–J11 đều đã có quyết định cuối D-DESIGN7-1..11 trong `REVIEW-SYNTHESIS-DESIGN.md` và đã áp vào bản này qua F7D-01..F7D-25. Bảng dưới đây là bản ghi quyết định cuối, không còn điểm nào mở.

### 14.1 Bảng quyết định đã chốt J1–J11

| # | Điểm | Quyết định cuối | Tham chiếu | Phiếu (tóm tắt) |
|---|---|---|---|---|
| J1 | Pairing qua mạng có vi phạm `docs/pairing.md` không? | CHẤP NHẬN phân biệt: cấm "xin secret qua mạng" giữ nguyên văn; handshake xác thực lẫn nhau có SAS ngoài băng được phép, không secret nào qua mạng (§3.1). | D-DESIGN7-1 | Dola: đồng ý. ChatGPT: đồng ý. |
| J2 | Diễn giải chữ P7.3 về cert đã pin thành pin khóa P-256 thô + AEAD? Đường xác thực bằng token cho driver giữ hay cắt? | CHẤP NHẬN diễn giải pin khóa thô + AEAD; ĐỔI TÊN đúng bản chất ở mọi nơi; CẮT đường xác thực bằng token cho driver khỏi v1 (§5.2). | D-DESIGN7-2 | Dola: đồng ý + ý kiến khác (giữ đường token, tắt mặc định — ghi ở bảng sign-off). ChatGPT: đồng ý có điều kiện (cắt token + đổi tên). |
| J3 | Khóa thiết bị lưu file 0600 thay vì OS keyring? | CHẤP NHẬN file 0600 cho v1, kèm văn bản cảnh báo mất khóa/không backup (§3.2, §12.3). | D-DESIGN7-3 | Dola: đồng ý. ChatGPT: đồng ý có điều kiện (cảnh báo mất khóa). |
| J4 | Versioning: Lamport + device_id hay vector clock? | GIỮ Lamport + device_id; BẮT BUỘC thuật toán phát hiện concurrent chính xác (§4.4); bác quy tắc đơn giản "bằng nhau → conflict, lớn hơn → cập nhật"; cấm mọi suy luận "Lamport lớn hơn thắng". | D-DESIGN7-4 | Dola: đồng ý giữ Lamport + yêu cầu thuật toán (quy tắc đơn giản của Dola bị bác). ChatGPT: thay đổi theo hướng giữ Lamport + thuật toán sync state. |
| J5 | Todos/plans: schema tối thiểu + không tool model sửa? | CHẤP NHẬN schema tối thiểu + không tool model sửa; bổ sung ghi chú mở rộng schema có version + hướng dẫn không ghi bí mật vào trường tự do (§4.6). | D-DESIGN7-5 | Dola: đồng ý. ChatGPT: đồng ý. |
| J6 | Audit attribution: ghi trong `detail` hay trường first-class? | CHẤP NHẬN ghi trong `detail`; không thêm trường first-class vào `AuditRecord` trong Phase 7 (§6.3). | D-DESIGN7-6 | Dola: đồng ý. ChatGPT: đồng ý. |
| J7 | Phê duyệt từ peer: mặc định TẮT theo peer hay BẬT cho owner? | CHẤP NHẬN mặc định TẮT theo peer, user bật rõ từng peer (§6.2). | D-DESIGN7-7 | Dola: đồng ý. ChatGPT: đồng ý. |
| J8 | WS server: tự triển khai framing tối thiểu hay xin dep tungstenite? | GIỮ tự triển khai tối thiểu + điều kiện bắt buộc: liệt kê chính xác phần RFC 6455 triển khai/bị từ chối, cổng interop + frame-vector + fuzzing bắt buộc, thất bại thì leo thang, không tự thêm dep (§8.2). | D-DESIGN7-8 | Dola: đồng ý tự triển khai. ChatGPT: không đồng ý hoàn toàn, muốn tungstenite (parent giữ tự triển khai theo ưu tiên 0-dep + điều kiện cứng). |
| J9 | Đường XiaoZhi v1: lab-only + ép WS + không MQTT? | CHẤP NHẬN lab-only + ép WS; viết lại §8.2 sạch; sửa CRITERIA P7.5 cho nhất quán qua nhật ký độ lệch (§8.2, §14.3). | D-DESIGN7-9 | Dola: đồng ý có điều kiện. ChatGPT: đồng ý có điều kiện (làm rõ ngoại lệ ws://). |
| J10 | Ranh giới địa chỉ: chỉ private/tailnet, từ chối public tuyệt đối? | CHẤP NHẬN ranh giới private/tailnet, từ chối public tuyệt đối, không ngoại lệ cấu hình (§9.2). | D-DESIGN7-10 | Dola: đồng ý. ChatGPT: đồng ý. |
| J11 | SAS: gõ mã hay hai bên cùng hiển thị + so sánh? | THAY ĐỔI: luồng chính = hiển thị SAS hai bên + đối chiếu + xác nhận riêng từng phía; luồng gõ tay là fallback cho thiết bị không màn hình (§3.3). | D-DESIGN7-11 | Dola: thay đổi (hai bên cùng hiển thị). ChatGPT: thay đổi (ưu tiên hai bên cùng hiển thị, giữ fallback cho thiết bị không màn hình). |

### 14.2 Mâu thuẫn / mơ hồ trong CRITERIA — đã giải quyết như sau

1. **P7.3 "chữ về cert đã pin" vs cây không có sinh cert:** đã giải quyết tại J2/D-DESIGN7-2 — diễn giải thành pin khóa thô + AEAD, đổi tên đúng bản chất, xem nhật ký độ lệch mục 1.
2. **P7.1 "lưu trong vault từng OS (FileVault/KeychainVault đã có)" vs `KeychainVault` chỉ đọc:** đã giải quyết tại J3/D-DESIGN7-3 — file 0600 cho v1 + cảnh báo mất khóa, xem nhật ký độ lệch mục 2.
3. **Tập sync gồm "todos/plans" nhưng cây không có todos/plans:** đã giải quyết tại J5/D-DESIGN7-5 — schema tối thiểu mới + mở rộng có version, xem nhật ký độ lệch mục 4.
4. **P7.5 "TLS/WSS only, cert pinning" cho skeleton v2 vs đường v2 dùng AEAD thuần (§8.3):** đã giải quyết cùng J2/J9 (D-DESIGN7-2, D-DESIGN7-9) — "TLS/WSS only" áp dụng khi firmware dùng TLS; đường v2 dùng pin khóa + AEAD, xem nhật ký độ lệch mục 3.
5. **G1 "650 cũ":** đã giải quyết — đếm thật tại HEAD là 662 thuộc tính test (§0.5); G1 hiểu là "toàn bộ test tại HEAD + mới", xem nhật ký độ lệch mục 5.
6. **P7.0 mục 8: "audio UDP không xác thực là giới hạn đã biết HOẶC ép WebSocket":** đã giải quyết tại J9/D-DESIGN7-9 — chọn ép WebSocket, không MQTT/UDP audio trong v1.
7. **"Đa kênh desktop" (§ đầu CRITERIA):** đã giải quyết — hiểu đúng như CRITERIA (P0-13 + P0-18), không mở rộng (§0.1).
8. **`pairing.md` cấm pairing qua mạng vs P7.1 yêu cầu pairing ceremony qua mạng:** đã giải quyết tại J1/D-DESIGN7-1 — phân biệt "xin secret qua mạng" (cấm, giữ nguyên văn) với "handshake + SAS ngoài băng" (cho phép), §3.1.

### 14.3 Nhật ký độ lệch CRITERIA (F7D-25)

Mỗi mục ghi: chữ CRITERIA → quyết định cuối → J tham chiếu.

| # | Chữ CRITERIA | Quyết định cuối trong bản này | J |
|---|---|---|---|
| 1 | P7.3: "mTLS với cert đã pin" | Pin khóa P-256 thô + AEAD, xác thực lẫn nhau bằng khóa danh tính đã pin + ECDH ephemeral + AEAD (§5.2); TLS + pinning Phase 4 vẫn dùng nơi peer đã có TLS. | J2 (D-DESIGN7-2) |
| 2 | P7.1: "lưu trong vault từng OS (FileVault/KeychainVault đã có)" | File 0600 trong v1 (`DeviceKeyStore`, §3.2) + cảnh báo mất khóa/không backup; keyring OS là cải tiến tương lai. | J3 (D-DESIGN7-3) |
| 3 | P7.5: "TLS/WSS only, cert pinning" cho skeleton v2 | Đường v2 dùng AEAD thuần với pin khóa (§8.3); "TLS/WSS only" áp dụng khi firmware dùng TLS (đường XiaoZhi). | J9 (D-DESIGN7-9) |
| 4 | Tập sync gồm "todos/plans" trong khi cây chưa có store | Schema tối thiểu mới do thiết kế định nghĩa (§4.6), mở rộng có version, không tool model sửa. | J5 (D-DESIGN7-5) |
| 5 | G1: "650 test" | Toàn bộ test tại HEAD (đếm thật 662 thuộc tính test, §0.5) + test mới (100 test nêu tên, §11), 0 fail. | — (§0.5) |
| 6 | P7.5: "test chứng minh OTA không bao giờ trả ws://" | Luật OTA mới (§8.2): mặc định chỉ URL an toàn + ngoại lệ lab duy nhất (`lab_mode=true`, loopback/tailnet); test `ota_only_safe_urls` khẳng định đúng luật mới gồm cả ca lab riêng. | J9 (D-DESIGN7-9) |

### 14.4 Bảng SIGN-OFF

| Reviewer | Phạm vi | Verdict | Ghi chú |
|---|---|---|---|
| Dola | Q1–Q3 (kiến trúc), theo bản tóm tắt | APPROVE-WITH-CHANGES | Chat: dola.com/38418647016103441. 0 Blocker, 4 Major, 3 Minor. Ý kiến khác ghi nhận: Dola muốn giữ đường token driver, tắt mặc định, chỉ quyền guest — đã quyết CẮT theo D-DESIGN7-2 (§5.2). Quy tắc phát hiện concurrent đơn giản của Dola ("bằng nhau → conflict, lớn hơn → cập nhật") bị bác theo D-DESIGN7-4 (§4.4). |
| ChatGPT (GPT-6) | Q4–Q6 (UX), theo bản tóm tắt | APPROVE-WITH-CHANGES | Chat: chatgpt.com/c/6afe189f-5cd0-83ec-a666-0423d59162e9. 1 Blocker (trust hai phía), 8 Major, 5 Minor. Không đồng ý hoàn toàn về J8 (muốn tungstenite) — parent giữ tự triển khai + điều kiện cứng theo D-DESIGN7-8. |
| Gemini/Hydrogen | Q7–Q8 (red-team/hiệu năng) | REQUEST-CHANGES — KHÔNG ĐẾM | Chat: gemini.google.com/app/c677a40b42f78506. Lý do không đếm: bài mô tả một thiết kế khác với bản nháp + tự định nghĩa lại J/H theo nghĩa khác + không trả lời đúng Q7/Q8 của brief; xem `REVIEW-SYNTHESIS-DESIGN.md` §0 (truth-check có bằng chứng). Chỉ phần cứu được (B1 trùng H5 + 3 ghi chú chéo) đã đưa vào F7D-05/F7D-23. Argon không có trong bộ chọn model của phiên đó. |
| Claude | — | KHÔNG CÓ BÀI | Không còn cửa sổ sống; user chưa trả lời câu hỏi khôi phục phiên; không thử đăng nhập lặp lại theo đúng quy tắc. Dola phủ Q1–Q3 thay Claude. |

- Ngày sign-off: 19/11/2026. HEAD xuất phát: `a431c42` (`a431c428ac46456b553a6928878f22e104ce424b`).
- **Khoảng trống red-team Q7/Q8 (nói thật):** không reviewer nào trả lời đúng Q7 tấn công của brief (Gemini trả lời sai câu hỏi; Dola Q1 chỉ xác nhận ở mức cao; ChatGPT ngoài phạm vi). Vòng review code Phase 7 PHẢI hỏi lại Q7 trên code thật; cổng G2/G3 của CRITERIA (test tấn công §2.6/§3.8/§5.5/§6.4/§7.4 + quét âm) là bù đắp bắt buộc, không được coi red-team thiết kế là đã đủ. Tương tự Q8: ngân sách §10 đứng trên số đo probe; cổng G2 đo thật B1–B8 mới là câu trả lời.
- Điều kiện sign-off (synthesis §4): đủ F7D-01..F7D-25 đã áp; bảng sign-off đầy đủ, ghi trung thực cả bài Gemini không đếm + Claude vắng; CẤM commit code Phase 7 trước khi xong; CẤM động README gốc + assets; coordinator đối chiếu độc lập từng F7D.

---

## Phụ lục: danh mục bằng chứng

- `design-evidence/README.md` — cái gì đã chứng minh / chưa + cách chạy lại.
- `design-evidence/ring-probe/Cargo.toml`, `ring-probe/src/main.rs`, `ring-probe/Cargo.lock` — source probe crypto (pin `ring = "=0.17.14"`); bản trên vega: `/home/adminis/mimint-p7-ring-probe/`.
- `design-evidence/ring-probe-run.log` — log chạy probe trên vega (PASS, exit 0, kèm bench).
- `design-evidence/probe-bc-api-audit.sh`, `design-evidence/api-audit.log` — script + log đối chiếu API pinning/TLS/vault/profile/notebook/audit/guards (chỉ đọc); bản trên vega: `/home/adminis/mimint-p7-probe-bc.sh`.
- Cây chỉ-đọc phục vụ đọc code giai đoạn 1: `/home/adminis/mimint-p7-design-read` tại `a431c428ac46456b553a6928878f22e104ce424b` (clone mới từ remote `main`; cây `/home/adminis/mimint` cũ ở `f178318` không được dùng).
- Nguồn quyết định: `CRITERIA.md` (cùng thư mục), `SYNTHESIS-PLAN.md` §5–§9, `PROPOSALS-SYNTHESIS.md`, `phase0/lane2-tls-wss.md`, `GAP-CHECK-2-REPO.md`, `gap-lane2-xiaozhi.md`, `phase6/DESIGN-DRAFT.md` + `DESIGN-FINAL.md` (mẫu cấu trúc).
- Nguồn review: `REVIEW-SYNTHESIS-DESIGN.md` (quyết định D-DESIGN7-1..11, F7D-01..F7D-25, sign-off), `design-review-dola.md`, `design-review-chatgpt.md`, `design-review-gemini.md` (không đếm, xem synthesis §0).

— Hết bản thiết kế cuối Phase 7. Đã review, đã sign-off ngày 19/11/2026. —

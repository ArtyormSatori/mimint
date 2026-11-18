# Thiết kế Phase 6 — MCP + execute_code + sub-agent song song (BẢN CUỐI)

- **Trạng thái:** BẢN CUỐI ĐÃ REVIEW — sign-off tại §14. Cổng G6-10: không commit code
  Phase 6 nào trước khi bản này có sign-off (đã đủ điều kiện §5 của tổng hợp review:
  đủ F6D-01..F6D-24, proof sandbox chạy lại với bộ cờ mới, bảng sign-off đủ).
- **Ngày soạn bản nháp:** 2026-11-18. **Ngày chốt bản cuối:** 2026-11-18.
  **Người viết duy nhất (giai đoạn 2, bước 1):** vòng viết thiết kế cuối, áp toàn bộ
  danh sách sửa F6D-01..F6D-24 theo `REVIEW-SYNTHESIS-DESIGN.md`. Không viết code Phase 6.
- **HEAD xuất phát:** `b48dae9241659b0fea9aa80b5b8bfb258229a0ab` (Phase 5 đã đóng,
  419 test; đã xác nhận bằng `gh api` trên vega; cây staging tại
  `/home/adminis/mimint-phase6-design`, chưa commit, coordinator đối chiếu rồi tự push).
- **Nguồn bắt buộc:** `CRITERIA.md` (P6.0–P6.6), `docs/phase6-gates.md` (G6-1..G6-10) tại
  HEAD, `SYNTHESIS-PLAN.md` §8 Phase 6 + §11 cơ chế Agent S số 4 (định tuyến GUI/Code) và
  số 5 (hierarchy tùy chọn có đo độ trễ), `DESIGN-DRAFT.md` (bản nháp 1.056 dòng),
  `REVIEW-SYNTHESIS-DESIGN.md` (7 quyết định D-DESIGN-1..7 + 24 mục sửa F6D-01..F6D-24),
  3 bài review toàn văn (`design-review-chatgpt.md`, `design-review-gemini.md`,
  `design-review-dola.md`).
- **Quy tắc không nhập (Agent S):** KHÔNG BAO GIỜ chạy code không sandbox. Mọi khẳng định
  về sandbox trong bản này đều dựa trên log kiểm chứng ở §0.3, không suy đoán. Mọi khẳng
  định "đã chứng minh" trong bản này đều có log/exit code đi kèm; cái gì chưa chứng minh
  được thì ghi rõ là chưa chứng minh, kèm cổng mở ở §13.

## 0. Phạm vi, nguyên tắc, bằng chứng

### 0.1 Phạm vi

Bản thiết kế quyết: định danh agent, lập lịch, bản đồ trạng thái chia sẻ, quyết định G6-9,
sandbox `execute_code` theo OS, hợp đồng MCP client, luật định tuyến GUI/Code, hierarchy tùy
chọn + đo độ trễ, hàng đợi + SLA, hủy level-3, mô hình trạng thái UI đa agent (§15), giới
hạn tài nguyên giữa các agent (§16). Mọi mục ở §13 (BỊ CHẶN / còn mở) bản này KHÔNG quyết
thay. Các mục review yêu cầu sửa được đánh dấu `(F6D-xx)` / `(D-DESIGN-n)` ngay tại chỗ áp
dụng, để coordinator đối chiếu từng mục.

### 0.2 Nguyên tắc (kế thừa cây hiện tại, không phát minh lại)

1. **Một body, một actor.** Body actor vẫn là điểm tuần tự hóa duy nhất (G6-1). Không agent
   nào sở hữu body riêng.
2. **Deny-by-default, fail-closed.** Mọi quyết định mơ hồ đều nghiêng về từ chối + ghi audit.
   Audit ghi không được thì việc không chạy (tiền lệ `AuditLog`, `guards.rs`).
3. **Không thay đổi hành vi single-agent khi cờ tắt** (G6-8). Mọi thay đổi phải chứng minh
   bằng suite cũ chạy không đổi. Ngoại lệ duy nhất được phép: file audit JSONL thêm trường
   `agent_id` (§1.4) — chấp nhận theo D-DESIGN-7, với tương thích schema đầy đủ: record cũ
   thiếu `agent_id` vẫn đọc được (mặc định `agent-001` khi deserialize thiếu trường), record
   mới bắt buộc có, quy ước `agent-000` cho hành động hệ thống, viewer giấu cột agent khi
   chỉ có một agent, test đọc cả fixture JSONL cũ lẫn mới (§1.4).
4. **Bằng chứng bằng máy.** Mọi cơ chế mới đều có pass test nêu tên ở §11. Không test thì
   không coi là có.
5. **0 dependency mới.** MCP client và sandbox dùng `std::process` + `serde_json` đã có
   trong cây. Chỉ xin thêm 2 feature của tokio đã có (`sync`, `time`) cho hủy level-3 (§10),
   0 package mới, license audit không đổi. Mọi dependency khác đều ngoài phạm vi bản này.

### 0.3 Bằng chứng kiểm chứng sandbox trên vega (bước bắt buộc)

Máy: Debian 13 (trixie), kernel `6.12.113+deb13-amd64`, user `adminis` (uid 1000), không sudo.
Toàn bộ log nằm ở `design-evidence/` (bản local, lấy bằng ssh redirect):

| Log | Nội dung | Kết quả |
|---|---|---|
| `01-env-probe.log` | `bwrap --version`, sysctl userns, seccomp, kernel config | bwrap **0.12.0** tại `/usr/bin/bwrap`, không setuid; `unprivileged_userns_clone=1`; `max_user_namespaces=63147`; `CONFIG_USER_NS=y`, `CONFIG_SECCOMP=y`, `CONFIG_SECCOMP_FILTER=y`; `libseccomp.so.2.6.0` có mặt; **không thấy `CONFIG_LANDLOCK`** trong `/boot/config` |
| `02-userns-probe.log` | `unshare -Urm`, `-Urnm`, mount tmpfs, bind-mount | U1–U4 đều pass; netns mới chỉ còn `lo` (0 packet) |
| `03-sandbox-proof.log` | Proof 3 ca bằng bwrap `--unshare-all` (bộ cờ cũ) | **A: sentinel bị từ chối** (exit 1); **B: mạng bị chặn** (`Errno 101`, curl exit 7) trong khi baseline ngoài sandbox kết nối `1.1.1.1:80` thành công; **C: file cho phép đọc được** (exit 0); file ghi vào HOME trong sandbox không tồn tại ngoài sandbox |
| `04-sandbox-proof-v2.log` | **Proof chạy LẠI với bộ cờ MỚI** (§5.1): baseline + 3 ca A/B/C + kiểm chứng mới D/E/F (F6D-01, F6D-03, F6D-04) | Baseline ngoài: sentinel HOME + sentinel /tmp + file cho phép đều đọc được (exit 0), nối `1.1.1.1:80` thành công. **A: DENIED-PASS** (exit 1). **B: chặn** (python `Errno 101` exit 1, curl exit 7). **C: đọc/ghi `/work` thành công** (exit 0), ghi vào HOME không tồn tại ngoài. **D: DENIED-PASS** (sentinel /tmp exit 1, `/tmp/.X11-unix` không tồn tại). **E1: PASS** (chỉ stdio + fd script dự kiến, không fd 9, không target cấm). **E2: HAZARD-CONFIRMED** (fd 9 kế thừa đọc được sentinel — bwrap KHÔNG đóng fd lạ, xem §5.1). **F: CAPPED-PASS** (ghi 80 MiB vào tmpfs 64 MiB: dd exit 1, file đúng 67108864 byte). **F2: CAPPED-PASS** (prlimit: FSIZE/AS/NPROC nhìn thấy trong sandbox; ghi vượt FSIZE 1 MiB: dd exit 153 SIGXFSZ, file đúng 1048576 byte) |
| `05-rlimit-scope.log` | Probe bổ sung: phạm vi RLIMIT_NPROC + ép RLIMIT_AS (F6D-04) | **AS: ép được** (giới hạn 256 MiB: cấp 128 MiB OK, cấp 512 MiB → `MemoryError`). **NPROC: KHÔNG đặt chặt được trước bwrap** — `NPROC=32` và `NPROC=200` đều làm bwrap chết ngay lúc tạo namespace (`Resource temporarily unavailable`, exit 1) trong khi uid đã có ~182 tiến trình; xem §5.1 |
| `probe-01-env.sh`, `probe-02-userns.sh`, `probe-03-proof.sh`, `probe-03-inner.sh`, `probe-04-proof.sh`, `probe-04-inner.sh`, `probe-05-rlimit-scope.sh`, `probe-05-inner.sh` | Script tái lập (bản trên vega: `/home/adminis/mimint-p6-probe-*.sh`, inner v1 `/home/adminis/mimint-p6-inner.sh`, inner v2 `/home/adminis/mimint-p6-inner-v2.sh`, inner 05 `/home/adminis/mimint-p6-inner-05.sh`) | Chạy lại được, không sudo, tự dọn sentinel/workdir |

Kết luận duy nhất được phép rút ra: **trên vega, bubblewrap + user namespace không đặc quyền
với đúng bộ cờ §5.1 là cơ chế sandbox đã chứng minh được**, gồm giới hạn tmpfs `--size`
64 MiB và kế thừa/ép RLIMIT_FSIZE + RLIMIT_AS qua lớp bọc ngoài bwrap. Ba giới hạn đã chứng
minh bằng thực nghiệm cũng phải ghi rõ: (a) bwrap không đóng fd kế thừa — kỷ luật spawn
phải tự lo (E2); (b) RLIMIT_NPROC chặt trước bwrap phá vỡ việc tạo namespace — không đặt
(05); (c) mọi cơ chế khác (seccomp filter, Landlock, macOS / Windows / Android) đều ở trạng
thái chưa kiểm chứng trong bản này (§5).

### 0.4 Điểm chạm code hiện có (đọc thật tại HEAD, tóm tắt)

| Module | Sự thật đã đọc (làm nền cho thiết kế) |
|---|---|
| `crates/mimint-core/src/brain.rs` | Invariant dòng 1–6: *"Tools run one at a time, in order. Parallelism is Phase 6."* `Brain` mượn `provider`/`registry`/`approver`/`audit`, sở hữu `turn_id`, `max_steps`, `cancel_tokens`, `verifier`, `skills: Vec<SkillDef>` (snapshot theo lượt), `profile`. Vòng lặp: verify-previous + reflection bắt buộc mỗi bước; tool chạy nối tiếp theo thứ tự; hủy chỉ kiểm tra ở đầu bước / giữa các tool / qua guard executor. |
| `crates/mimint-core/src/guards.rs` | `GuardedExecutor` cấp `request_id` đơn điệu **theo từng executor** (`AtomicU64` từ 1); quyết định sai id → deny `approval_id_mismatch`; audit ghi hỏng thì fail-closed (`?`). `ApprovalRequest { id, tool, args }`, `ApprovalOutcome { request_id, decision }` — chưa có agent id. |
| `crates/mimint-core/src/audit.rs` | `AuditLog`: `Mutex<File>`, append-only, ghi theo dòng nguyên vẹn dưới tải đồng thời (đã có test 8 thread × 25 ghi). `AuditRecord { ts_ms, turn_id, kind, tool, decision, detail }` — chưa có agent id; `profile` hiện gắn trong `detail`. Deserialize thiếu `agent_id` phải mặc định `agent-001` (D-DESIGN-7, §1.3). |
| `crates/mimint-core/src/cancel.rs` | `CancelToken` là cờ hợp tác, latch, không un-cancel. Tài liệu ghi rõ: KHÔNG ngắt HTTP đang bay; hủy level-3 chưa có. Phase 6 thêm waiter async (`Notify`) + đẩy hủy vào actor/approver (§10, F6D-02). |
| `crates/mimint-control/src/actor.rs` | `BodyActor`: 1 thread riêng sở hữu body, kênh mpsc, timeout mỗi action mặc định **30s** (`DEFAULT_ACTION_TIMEOUT`); timeout / chết thread / panic → `BodyError::UnknownOutcome` + latch `broken` vĩnh viễn (fail-fast, phải dựng actor mới); probe timeout KHÔNG làm hỏng actor; `StopFlag` kiểm tra trước mỗi action; panic trong loop thì bắt, báo `UnknownOutcome`, dừng loop. Phase 6 thêm đường nhận tín hiệu rút/hủy từ coordinator (§10, F6D-02). |
| `crates/mimint-body/src/guarded.rs`, `body.rs` | `StopFlag` latch, first-trip-wins `TripInfo { source, reason, ts_ms }`, không untrip, chưa có agent. `TargetSnapshotSlot`: **1 slot duy nhất** cho cả worker ("only one approval waits at a time") → thành map theo agent + dọn dẹp khi reap (§3.1, F6D-09). |
| `crates/mimint-control/src/tools.rs`, `frames.rs` | TTL quan sát mặc định **300s** (`DEFAULT_MAX_OBSERVATION_AGE`); re-verify qua snapshot slot; `FrameStore` có `latest_id`/`latest_age` **toàn cục**, `FrameMeta` chưa có agent; purge tainted toàn cục. Phân vùng frame là attribution + TTL, KHÔNG phải cô lập hiển thị (F6D-07, §3). |
| `crates/mimint-ui/src/approver.rs` | `UiApprover` qua kênh mpsc, 1 thẻ tại 1 thời điểm theo thứ tự; timeout thẻ mặc định **60s**; `ApprovalJob { id, tool, args, deadline, timeout, target, observation_age, max_observation_age, reply }` — chưa có agent; cờ pending dùng chung với guard chống tự phê duyệt. Phase 6 thêm đường rút thẻ khi agent bị hủy (§10, F6D-02) + nội dung thẻ đa agent (§15.2, F6D-17). |
| `crates/mimint-ui/src/worker.rs` | **1 worker thread** duy nhất, sở hữu tokio runtime + chain + registry; `block_on(brain.run(...))` nối tiếp từng lượt; `turn_id = "ui-turn-{pid}-{ms}"`; panic trong lượt được `catch_unwind` → `WorkerFault`, worker vẫn phục vụ; token lượt mới mỗi lượt qua `TokenSlot`, token stop dùng chung. |
| `crates/mimint-core/src/registry.rs`, `provider.rs`, `fallback.rs` | `CostCap` chỉ là **cổng opt-in, KHÔNG đo chi tiêu** (ghi rõ trong code + `docs/privacy-and-cost.md`: usage không parse, đô la không đo) — xuất hiện tường minh trong bản đồ trạng thái (§3, F6D-11). `Provider` stateless, `Send + Sync`; `CallContext { turn_id, audit, deadline, profile }` — cố tình không có token hủy; chain có ngân sách mỗi lần gọi (mặc định 120s). |
| `crates/mimint-core/src/egress.rs` | `GuardedHttpClient` (reqwest): kiểm tra URL trước mọi I/O, Privacy ON = chỉ 3 dạng loopback, không redirect, không proxy; timeout gắn sẵn, **không có cơ chế abort**. Abort bằng drop future là giả định phải đo ở P6.4, có quy tắc dự phòng (§10, F6D-12). |
| `crates/mimint-core/src/notebook.rs`, `memory.rs`, `skills.rs`, `soul.rs` | Notebook: 1 bảng SQLite `notes`, `Mutex<Connection>`, không cột owner/shared — cơ chế chống lost update đã có là mutex single-writer + chỉ owner sửa dòng shared (§3.2, F6D-10). `ExperienceStore`: trajectory có provenance checker + skill, không cột agent — Phase 6 chỉ `agent-001` commit, trajectory agent-002+ quarantine (F6D-07, §3). Skills: snapshot `Vec<SkillDef>` mỗi lượt; store file-based. SOUL: prompt hệ thống clone mỗi lượt qua `set_system_prompt`. |
| `crates/mimint-core/src/config.rs` | `FileConfig` dùng `deny_unknown_fields` — mọi section mới (`[agents]`, `[mcp-server]`, `[sandbox]`, `[hierarchy]`, `[routing]`) phải khai báo tường minh (§12). |

---

## 1. Định danh agent

### 1.1 Quyết định

- **Định dạng:** `agent-NNN`, `NNN` là số thứ tự thập phân đệm 0 thành 3 chữ số, bắt đầu từ
  `001`. Chuỗi id là bất biến (immutable `String`, không parse ngược thành số ở đường nóng).
- **Id dành riêng:** `agent-000` = hệ thống/coordinator (bản ghi không thuộc lượt nào: khởi
  động, đổi cấu hình, rebuild, emergency stop không gắn agent). `agent-001` = agent mặc định
  (agent chính, duy nhất khi cờ song song tắt). `agent-002+` = agent sinh thêm.
- **Ai cấp:** `AgentCoordinator` (module mới, thuộc worker — xem §2.4) là nơi duy nhất cấp id,
  từ một `AtomicU64` trong tiến trình, bắt đầu ở 1 (`agent-001` tạo sẵn lúc khởi động).
  Không module nào khác được tự đặt id.
- **Vòng đời:** `Created` (spawn, cấu hình đã validate) → `Running` (đang chạy lượt) →
  `Done` | `Cancelled` | `Failed` (kết thúc, task đã join + kiểm chứng, §10) → `Reaped`
  (id nghỉ hưu, **không tái sử dụng trong tiến trình**; lúc reap coordinator dọn slot
  snapshot của agent — §3.1, F6D-09). Id sau khởi động lại bắt đầu lại từ `001`; phân biệt
  giữa các lần chạy bằng bản ghi khởi động + `turn_id` chứa pid/ms (§1.3).
- **Số lượng:** tổng số agent đồng thời tối đa theo cấu hình, mặc định **3** (chính + 2),
  khoảng cho phép 1..8, sai thì lỗi cấu hình fail-closed. Lý do: máy chuẩn yếu (vega), body
  vốn đơn luồng — song song chỉ có ích cho thời gian chờ provider, không phải thông lượng body
  (xem thêm ghi chú thực tế §9.1 và giới hạn tài nguyên §16, F6D-20/F6D-23).
- **Tên hiển thị:** `agent-001` hiển thị như hôm nay (không thêm nhãn). Agent sinh thêm hiển
  thị `Agent N · <nhãn tác vụ>` (nhãn do lệnh spawn cung cấp, cắt 40 ký tự, chỉ dùng để hiển
  thị, không dùng làm khóa). Thẻ phê duyệt ghi tên agent khi cờ song song bật (G6-2); khi tắt,
  thẻ hiển thị y hệt hôm nay. Mô hình trạng thái hiển thị đầy đủ ở §15 (F6D-16).

### 1.2 Lý do

- Id dạng chuỗi có thứ tự, cấp tập trung: không va chạm giữa các agent, dễ gắn vào audit và
  dễ đọc trong log. Không dùng UUID: không cần chống va chạm liên máy (Phase 7 mới có đồng bộ
  liên body, ngoài phạm vi), và id ngắn giúp thẻ/audit dễ đọc.
- `agent-000` tách riêng để G6-4 ("mọi bản ghi đều có agent id") không ép bản ghi hệ thống
  phải mạo danh một agent.
- Giới hạn 3 mặc định là quyết định hiệu năng có chủ đích cho máy yếu, không phải số bịa:
  mỗi agent thêm là một task provider + hàng đợi body dài thêm; đo độ trễ ở §8 sẽ kiểm chứng
  lại con số này.

### 1.3 Điểm chạm code hiện có

- `AuditRecord` (`audit.rs`): thêm trường first-class `agent_id: String` (không nhét vào
  `detail` như `profile`, vì G6-4 cần lọc theo agent và vì "không có agent id thì không tạo
  được bản ghi" phải ép bằng kiểu). `AuditLog::log` giữ chữ ký cho đường hệ thống (ghi
  `agent-000`); thêm `log_for(agent_id, ...)` cho đường agent. `Brain::log` và
  `GuardedExecutor::log` chuyển sang `log_for`.
- **Tương thích schema (D-DESIGN-7):** record cũ không có `agent_id` vẫn đọc được:
  deserialize thiếu trường thì mặc định `agent-001` (record cũ chỉ có thể do agent mặc định
  đơn độc tạo ra). Record mới bắt buộc có `agent_id` (mọi đường tạo bản ghi đều qua kiểu bắt
  buộc id; không có đường nào ghi record mới mà thiếu trường). Quy ước `agent-000` chỉ dùng
  cho hành động hệ thống (§1.1), không bao giờ dùng làm mặc định cho record thiếu trường.
  Viewer/audit reader giấu cột agent khi log chỉ chứa một agent (hiển thị như hôm nay).
- `Brain::new` (`brain.rs`): thêm tham số bắt buộc `agent_id` (não không có id thì không dựng
  được). `TurnOutcome` thêm `agent_id`. `BrainProgress` thêm `agent_id` ở mọi variant có ngữ
  cảnh lượt (để UI gán tiến trình đúng agent).
- `GuardedExecutor::new` (`guards.rs`): thêm `agent_id`; `ApprovalRequest`/`ApprovalOutcome`
  thêm `agent_id`; ràng buộc id trở thành cặp `(agent_id, request_id)` — xem §3 hàng approval.
- `CallContext` (`provider.rs`): thêm `agent_id` để `FallbackChain` gắn vào bản ghi
  `provider_attempt`/`provider_failure`.
- `turn_id`: đổi định dạng thành `ui-turn-{agent_id}-{pid}-{ms}-{seq}` (`seq` đơn điệu theo
  từng agent, do coordinator cấp). Lý do bắt buộc: định dạng cũ `ui-turn-{pid}-{ms}` có thể
  va chạm khi 2 agent bắt đầu cùng mili-giây. `turn_id` vẫn là chuỗi mờ với viewer (chỉ có
  `ui-startup`/`ui-config` là giá trị đặc biệt, giữ nguyên).
- `WorkerEvent` (`worker.rs`): các variant theo lượt (`TurnDone`, `TurnFailed`,
  `TurnCancelled`, `TurnProgress`, `TryingBackend`) thêm `agent_id`. Khi cờ tắt, UI bỏ qua
  trường này và hiển thị như hôm nay.

### 1.4 Kiểm chứng (test sẽ viết, giai đoạn 2)

- `agent_id_format_and_uniqueness`: spawn N agent → id tăng đơn điệu, đúng định dạng, không
  trùng, không tái sử dụng sau khi reap.
- `default_agent_is_001_and_invisible_when_flag_off`: cờ tắt → toàn bộ suite cũ pass, UI
  render (chat/thẻ/trạng thái) không xuất hiện nhãn agent mới; file audit có trường
  `agent_id` với giá trị `agent-001`/`agent-000` (ngoại lệ duy nhất được phép của G6-8, §0.2).
- `turn_id_unique_under_concurrent_spawn`: 8 agent spawn cùng lúc → mọi `turn_id` khác nhau.
- `audit_record_requires_agent_id`: không có đường code nào tạo `AuditRecord` mà thiếu id
  (kiểm bằng kiểu + grep cổng: mọi lời gọi `AuditLog::log` ngoài đường hệ thống đều qua
  `log_for`).
- `audit_legacy_records_readable` (D-DESIGN-7): fixture JSONL cũ (không có `agent_id`, nằm
  trong cây test) đọc được, mọi record nhận mặc định `agent-001`; fixture mới thiếu
  `agent_id` ở đường ghi mới bị từ chối; viewer giấu cột agent khi log chỉ có một agent và
  hiện cột khi có từ hai agent trở lên.

---

## 2. Thuật toán lập lịch

### 2.1 Quyết định

Lập lịch 2 tầng, tách bạch:

**Tầng 1 — lượt (turn-level):** mỗi agent là một `Brain::run` riêng, chạy như một tokio task
trên runtime của worker (runtime hiện có, `Runtime::new()` trong `worker_loop`). Mỗi agent có
`Registry` riêng (dựng từ các thành phần dùng chung — §3), `GuardedExecutor` riêng,
`CancelToken` lượt riêng, `StepVerifier` riêng. Task của agent được giữ trong `JoinHandle`;
coordinator join + kiểm chứng kết thúc (§10). Số worker thread của runtime = `max_agents + 2`
(cộng 2 cho I/O nền), đặt lúc khởi động theo cấu hình (giữ nguyên theo F6D-20; hiệu lực của
cam kết hủy ≤5s dựa vào đẩy hủy F6D-02, đo ở P6.4 — §10).

**Tầng 2 — body (action-level):** mọi action body của mọi agent xếp hàng qua **một**
`BodyActor` duy nhất. Actor giữ **một hàng đợi FIFO riêng cho từng agent** và phục vụ theo
**round-robin: mỗi vòng lấy đúng 1 action của 1 agent**, bỏ qua hàng đợi rỗng, quay vòng công
bằng. Trong 1 agent, thứ tự FIFO tuyệt đối. Một action, một khi bắt đầu, chạy tới cùng không
xen kẽ (bất biến hiện tại của `actor_loop` giữ nguyên — G6-1).

**Không có độ ưu tiên** trong Phase 6. Agent chính không được chen hàng. Lý do: ưu tiên tái
tạo starvation và làm SLA (§9) không dự đoán được; nếu cần ưu tiên, đó là thiết kế riêng có đo
lường, không phải Phase 6.

**Probe** (`ProbeTarget`) đi chung hàng đợi round-robin của agent yêu cầu (probe do approver
phát ra mang id của agent sở hữu request đó), giữ nguyên ngữ nghĩa hiện tại: probe timeout
không làm hỏng actor.

### 2.2 Lý do

- Round-robin theo agent (không phải FIFO toàn cục): FIFO toàn cục cho phép 1 agent lắm mồm
  lấp đầy hàng đợi và đói agent khác vô thời hạn. Round-robin cho chặn trên chờ đợi có thể
  tính được: agent chờ tối đa `(N-1)` action của người khác trước mỗi action của mình, với N
  là số agent đang có việc trong hàng đợi.
- Task tokio thay vì thread riêng cho mỗi agent: `Brain::run` đã là `async`, provider đã là
  `async`; giữ nguyên mô hình, chỉ thêm tính đồng thời. Tool `execute` vẫn đồng bộ (trait
  `Tool` không đổi) — với số agent bị chặn trên (≤8) và runtime thread dự phòng, việc 1 task
  chặn 1 thread trong lúc chờ actor là có giới hạn và đo được. Phương án `spawn_blocking`
  cho tool được cân nhắc và **loại** ở Phase 6: nó đổi trait/hợp đồng tool toàn cây mà lợi
  ích chưa đo được; ghi lại để xem xét khi đo độ trễ (§8) cho thấy tắc nghẽn thread.
- Không ưu tiên: đơn giản, công bằng, kiểm chứng được. Mọi "agent chính quan trọng hơn" đều
  là cảm tính cho tới khi có số đo.

### 2.3 Điểm chạm code hiện có

- `worker_loop` / `run_one_turn` (`worker.rs`): đường `block_on(brain.run(...))` nối tiếp
  hiện tại trở thành đường của `agent-001` khi cờ tắt (không đổi). Khi cờ bật, coordinator
  `spawn` mỗi lượt agent lên runtime và theo dõi `JoinHandle`; `run_one_turn` tách thành
  hàm dựng `Brain` theo agent (dùng lại toàn bộ wiring profile/SOUL/skill/verifier hiện có).
- `BodyActor` (`actor.rs`): kênh intake mpsc giữ nguyên (không chặn); logic phục vụ trong
  `actor_loop` đổi từ "lấy 1 request, chạy ngay" thành "lấy request vào hàng đợi theo agent,
  phục vụ round-robin". `Request` thêm `agent_id`, `enqueue_time`, `deadline` (§9). `broken`
  latch, `UnknownOutcome`, kiểm tra `StopFlag` trước mỗi action: giữ nguyên. Thêm đường nhận
  tín hiệu rút/hủy từ coordinator để trả `Err(Cancelled)` tức thì (§10, F6D-02).
- `BodyTool` (`control/tools.rs`): `act()` của tool mang `agent_id` của agent sở hữu registry
  (tool được dựng với id đó lúc đăng ký — xem §3). Không đổi trait `Tool`.

### 2.4 Coordinator (thành phần mới, phạm vi hẹp)

`AgentCoordinator` sống trong worker, sở hữu: bộ đếm id, bảng agent đang sống
(`agent_id → AgentHandle { join_handle, cancel_token, budget, spawned_at }`), và quyền sinh/
hủy agent. Coordinator KHÔNG chạy não, KHÔNG ra quyết định guard, KHÔNG chạm body trực tiếp.
Mọi spawn đều qua coordinator (kể cả `delegate` của hierarchy, §8) để giới hạn số lượng và
ngân sách được ép ở một chỗ. Coordinator dọn slot snapshot của agent lúc reap (§3.1, F6D-09)
và đẩy tín hiệu hủy vào actor + approver khi hủy agent (§10, F6D-02).

### 2.5 Kiểm chứng

- `g6_1_round_robin_no_interleaving`: 2 agent cùng lái input vào oracle (MockBody); luồng
  event ghi được chỉ gồm các action nguyên vẹn theo thứ tự nào đó, không xen event trong 1
  action; và thứ tự phục vụ luân phiên (không có 2 action liên tiếp của cùng 1 agent khi cả
  2 đều có việc chờ).
- `scheduler_starvation_bound`: agent A gửi 50 action, agent B gửi 1 action sau đó → B được
  phục vụ trong chặn trên `(N-1)` action (đo bằng số action của A chạy trước B ≤ 1 với N=2).
- `no_priority_bypass`: không tồn tại đường code cho phép 1 agent vượt hàng đợi (kiểm bằng
  review + test thứ tự với 3 agent).
- `flag_off_single_task`: cờ tắt → đúng 1 task lượt tại 1 thời điểm, hành vi `block_on` như cũ.

---

## 3. Bản đồ trạng thái chia sẻ

Một mục chỉ nằm ở đúng một cột. "CHIA SẺ" = một thể hiện dùng chung, mọi agent thấy cùng
trạng thái. "PHÂN VÙNG" = một vật chứa chung nhưng trạng thái tách theo agent, agent này
không thấy/không đổi được phần của agent khác trừ khi luật ghi rõ. "SAO CHÉP" = mỗi agent
giữ một bản sao chụp tại lúc spawn/lượt; thay đổi sau đó không lan sang bản sao đang chạy.

| # | Thứ | CHIA SẺ | PHÂN VÙNG | SAO CHÉP | Ghi chú quyết định |
|---|---|---|---|---|---|
| 1 | Body actor | ● | | | Một `BodyActor`, handle clone dùng chung; hàng đợi con theo agent + round-robin (§2). Latch `broken` dùng chung → xem G6-9 (§4). |
| 2 | Approval channel | ● | | | **Chia sẻ transport** (F6D-11): một kênh mpsc chung; **dữ liệu trong kênh phân vùng theo khóa** `(agent_id, request_id)` — job/quyết định/snapshot slot tách theo agent. Một thẻ hiển thị tại 1 thời điểm; các job khác chờ theo thứ tự đến. Xem chi tiết dưới bảng. |
| 3 | Stop latch | ● | | | Một `StopFlag` + một stop `CancelToken` chung: trip là dừng TẤT CẢ (G6-3). `TripInfo` thêm `agent_id: Option<String>` ghi agent gây trip (nếu có). Không untrip (giữ nguyên). |
| 4 | Audit log | ● | | | Một `AuditLog` (`Mutex<File>` đã nguyên vẹn theo dòng). Mọi bản ghi mang `agent_id` (§1). Thứ tự trong 1 lượt của 1 agent được giữ vì 1 agent không bao giờ tự ghi đồng thời với chính nó. Viewer lọc theo agent (§3.1). Record cũ thiếu id đọc được, mặc định `agent-001` (D-DESIGN-7). |
| 5 | Notebook | | ● | | Một file SQLite, nhưng mỗi dòng có `owner_agent_id` + `shared`. Dòng private: chỉ owner đọc/ghi. Dòng shared: mọi agent đọc, chỉ owner sửa/xóa. Dòng cũ (trước Phase 6): migrate thành owner `agent-001`, `shared=true` (ghi chú của user phải đọc được; không ai ngoài chính được sửa). Xem §3.2. Rò rỉ ID enumeration là giới hạn đã biết, chấp nhận ở Phase 6 (F6D-07). |
| 6 | Provider chain / egress | ● | | | Một `FallbackChain` stateless dùng chung; `CallContext` mang `agent_id` để ghi attempt. `EgressPolicy`/Privacy Mode là toàn cục, không bao giờ theo agent. Deadline + hủy level-3 thì theo agent (§10). |
| 7 | Cost ledger | | ● | | Module MỚI (§3.3): bộ đếm theo agent + cap toàn cục, check-and-spend nguyên tử dưới 1 Mutex. Ngân sách từng agent phân vùng trong ledger; cap toàn cục dùng chung. `CostCap` cũ (cổng opt-in) giữ nguyên, không thay thế — xem hàng 13 (F6D-11). |
| 8 | Frame store / snapshot | | ● | | Một `FrameStore` chung nhưng frame nào cũng mang `agent_id` người chụp; TTL/độ tươi tính THEO AGENT trên frame của chính agent đó. Snapshot slot: 1 slot THEO AGENT (thay 1 slot toàn cục), xóa khi agent kết thúc (F6D-09). Purge tainted vẫn toàn cục (hành động an toàn, không phải attribution). **Phân vùng frame là attribution + TTL, KHÔNG phải cô lập hiển thị** — mọi agent dùng chung 1 màn hình vật lý (F6D-07). |
| 9 | Skills | | | ● | Snapshot `Vec<SkillDef>` theo lượt như hiện tại (`set_skills`), mỗi agent một bản sao chụp lúc bắt đầu lượt. `SkillStore` chỉ đọc trong lượt; promote/demote/import do coordinator điều phối, không bao giờ giữa lượt. |
| 10 | Memory kinh nghiệm | ● | | | Một `ExperienceStore` chung, đọc chung. **Chỉ `agent-001` được commit trajectory vào kho chung; trajectory của agent-002+ bị quarantine** cho tới khi lượt cha được xác minh (hoặc không ghi chung) (F6D-07). Mọi trajectory ghi thêm `agent_id` người ghi (provenance); quarantine theo skill giữ nguyên. |
| 11 | Config / profile | | | ● | `ResolvedConfig` + `Profile` clone cho mỗi agent lúc spawn. Rebuild/đổi profile chỉ áp dụng cho lượt/spawn sau, không đột biến agent đang chạy. |
| 12 | SOUL | | | ● | Chuỗi system prompt clone mỗi lượt như hiện tại (`set_system_prompt`). `SoulStore` chỉ đọc trong lượt; lưu SOUL vẫn qua đường có lịch sử, do user/coordinator. Luật `soul_may_be_sent` đánh giá theo agent lúc spawn từ trạng thái privacy + endpoint dùng chung lúc đó. |
| 13 | CostCap (cổng opt-in cũ) | ● | | | **Tường minh trong bản đồ** (F6D-11): cổng opt-in toàn cục, không đo chi tiêu, giữ nguyên vai trò; không bị ledger thay thế. Ledger (§3.3) là cơ chế đo + ngân sách mới, tách khỏi cổng này. |

### 3.1 Chi tiết approval (G6-2)

- `ApprovalRequest`/`ApprovalOutcome` mang `agent_id`; executor từ chối mọi outcome mà
  `agent_id` HOẶC `request_id` lệch, với cùng lý do `approval_id_mismatch` (giữ chuỗi lý do
  cũ, thêm trường `outcome_agent_id` vào detail để chẩn đoán).
- `ApprovalJob` mang `agent_id` + nhãn hiển thị; thẻ ghi tên agent khi cờ bật. UI vẫn hiện
  **một thẻ tại 1 thời điểm** (`pending_job` duy nhất trong `app.rs`); các job khác nằm trong
  kênh mpsc theo thứ tự đến. Một agent chỉ có tối đa 1 approval đang chờ (do
  `approve_request` đồng bộ — bất biến này tự chặn độ sâu hàng đợi thẻ ≤ số agent sống).
  Nội dung thẻ đầy đủ (vị trí hàng đợi, nhãn mức độ ảnh hưởng, ước tính token, hash code…)
  ở §15.2 (F6D-17).
- Thẻ được trả lời sau khi agent của nó bị hủy → deny (executor kiểm tra token hủy của đúng
  agent đó sau khi approver trả về, cùng bước với quyết định guard — mở rộng kiểm tra
  `cancelled_after_approval` hiện có sang token theo agent). Ngoài ra, khi hủy agent,
  coordinator đẩy tín hiệu rút thẻ ngay vào `UiApprover` để thẻ đang chờ được rút tức thì
  thay vì chờ hết timeout (§10, F6D-02).
- Snapshot slot theo agent: `TargetSnapshotSlot` đơn lẻ được thay bằng map
  `agent_id → TargetSnapshotSlot` do worker sở hữu; approver và body tool của 1 agent chỉ
  chạm slot của agent đó. `take_for(x, y)` vẫn chỉ tiêu thụ snapshot đúng điểm của đúng agent
  (G6-7).
- **Vòng đời dọn dẹp slot (F6D-09):** slot snapshot theo-agent bị **xóa khi agent kết thúc**
  (`Done`/`Cancelled`/`Failed`), do coordinator dọn dẹp lúc reap; TTL vẫn áp dụng cho slot
  đang sống. Snapshot mồ côi không bao giờ được tiêu thụ: sau khi slot bị xóa, mọi
  `take_for` với id agent đó đều thất bại như khi không có snapshot. Thứ tự dọn dẹp: agent
  kết thúc → join + kiểm chứng (§10) → coordinator xóa slot → id nghỉ hưu (`Reaped`).
- Cờ `pending_flag` (chống tự phê duyệt) vẫn là MỘT cờ chung: bất kỳ thẻ nào đang chờ cũng
  trip stop nếu có input body — an toàn hơn là phân vùng.

### 3.2 Chi tiết notebook (G6-5)

- Migration SQLite: `ALTER TABLE notes ADD COLUMN owner_agent_id TEXT NOT NULL DEFAULT
  'agent-001'`, `ADD COLUMN shared INTEGER NOT NULL DEFAULT 1` cho dòng cũ. DB cũ mở được
  nguyên vẹn (tiền lệ: migration test của notebook). Dòng mới do agent tạo: `owner` = agent
  đó, `shared` = false trừ khi tool gọi ghi rõ chia sẻ.
- Tool notebook (`NotebookSearchTool`/`Read`/`Add` trong `tools.rs`, `default_registry`):
  tool mang `agent_id` của agent sở hữu registry; `search`/`get` lọc: dòng private của agent
  khác thì như không tồn tại (không báo "bị chặn", tránh rò rỉ sự tồn tại); `update`/`delete`
  dòng không phải của mình → lỗi `Notebook` + audit, không thực hiện.
- **Chống lost update — cơ chế đã có, ghi rõ (F6D-10):** không cần khóa mới. Hai cơ chế hiện
  có kết hợp đã loại lost update do tranh chấp: (a) `Mutex<Connection>` — SQLite single-writer,
  mọi thao tác notebook qua một mutex duy nhất nên không có hai writer đồng thời ở tầng DB;
  (b) luật chỉ-owner-sửa — dòng shared chỉ owner được sửa/xóa, nên mỗi dòng chỉ có đúng một
  writer hợp lệ theo luật; writer khác bị từ chối trước khi chạm DB. Không có merge, không có
  last-writer-wins âm thầm. Test bắt buộc: 2 agent ghi đồng thời (kết quả: mọi ghi thành công
  đều nguyên vẹn, không ghi nào mất âm thầm) + sửa dòng shared của agent khác bị từ chối
  (xem §3.4).
- **Notebook ID enumeration (F6D-07):** id dòng notebook đoán được/tuần tự cho phép agent dò
  sự tồn tại của dòng qua lỗi phân biệt — đây là rò rỉ thông tin nhỏ đã biết, **chấp nhận ở
  Phase 6**, không thiết kế lại ID. Giảm thiểu hiện có: dòng private của agent khác trả về
  "như không tồn tại" (không phân biệt "không có" với "bị chặn") nên dò ID không xác nhận
  được nội dung hay quyền sở hữu.
- Trạng thái lượt (messages, pending verification, reflection) vốn đã tách theo `Brain`
  (mỗi agent một `Brain`) — không cần thêm cơ chế, chỉ cần không dùng chung `Brain`.

### 3.3 Chi tiết cost ledger (G6-6) — module mới, vì hôm nay không có đo lường (D-DESIGN-4)

- **Đơn vị đo: token.** Mỗi lần gọi provider ghi nhận `usage` (prompt + completion tokens)
  khi provider trả về. `ProviderClient`/`ClaudeClient` hôm nay không parse `usage` → Phase 6
  thêm parse `usage` (OpenAI-style `usage.{prompt_tokens,completion_tokens}`,
  Claude-style `usage.{input_tokens,output_tokens}`).
- **Bốn loại dữ liệu tách bạch, không bao giờ trộn lẫn:**
  1. `measured usage` — token provider báo về trong `usage` của một response hoàn chỉnh.
     Đây là số duy nhất được gọi là "đã đo".
  2. `reserved tokens` — token đã trừ trước (reserve) cho một lần gọi đang bay hoặc đã kết
     thúc mà chưa quyết toán xong. Reserve không phải chi tiêu đã đo; nó là tiền đặt cọc.
  3. `unmeasured estimate` — số token ghi sổ khi một lần gọi kết thúc mà không có `usage`
     (xem quy tắc settle dưới đây). Mọi hiển thị/tổng hợp dùng số này đều phải gắn cờ
     `usage_unmeasured=true` và ghi rõ là ƯỚC TÍNH.
  4. `actual USD` — chỉ tồn tại khi có BOTH bảng giá theo model trong cấu hình AND `usage`
     đã đo. Thiếu một trong hai thì USD = unknown, không bịa, không suy ra từ reserve.
- **Reserve-before, settle-after, nguyên tử:** trước mỗi lần gọi, ledger (1 Mutex) kiểm tra
  và trừ trước một mức reserve (mặc định = `max_tokens` của lần gọi, tối thiểu 1.000 token);
  sau lần gọi, quyết toán theo quy tắc settle dưới đây. Kiểm tra cap theo agent VÀ cap toàn
  cục trong cùng một lần khóa → N agent không thể cùng vượt cap toàn cục.
- **`max_tokens` mặc định 4096** (đã là mặc định từ fix pass Phase 2 — giữ nguyên, không đổi):
  một lần gọi thiếu `usage` tối đa chỉ ghi sổ 4.096 token ước tính, không cạn sạch ngân sách
  sub-agent trong một lần. Reserve tối đa mỗi lần gọi = `max_tokens` của lần gọi đó.
- **Quy tắc settle — một lần quyết toán duy nhất cho mỗi attempt, chống double-count:**
  mỗi lần gọi provider có `attempt_id` duy nhất; ledger ghi đúng một bản ghi settle cho mỗi
  `attempt_id` (settle lần hai → lỗi audit, không đổi số). Retry là attempt mới với
  `attempt_id` mới + reserve mới, không bao giờ quyết toán lại attempt cũ.

  | Kết thúc attempt | Có `usage` không? | Settle |
  |---|---|---|
  | Response hoàn chỉnh | Có | Trừ theo `usage` thật (measured), hoàn phần reserve thừa. |
  | Response hoàn chỉnh | Không | Tính phí toàn bộ reserve, gắn `usage_unmeasured=true` (unmeasured estimate). Không bao giờ ghi 0 âm thầm. |
  | Timeout | Không (hoặc chỉ một phần) | **KHÔNG giải phóng toàn bộ reserve.** Provider có thể đã tiêu thụ token trước khi timeout. Toàn bộ reserve giữ lại dưới dạng unmeasured estimate, attempt gắn cờ `outcome_unknown=true`. Phần `usage` một phần (nếu có) được ghi nhận là measured, phần còn lại của reserve vẫn giữ làm estimate — tổng ghi sổ của attempt không vượt reserve, không double-count. |
  | Cancel (hủy thắng sau khi đã gửi request) | Không | Như timeout: giữ toàn bộ reserve làm unmeasured estimate, `outcome_unknown=true`. Ranh giới chính xác: reserve chỉ được lấy ngay trước khi gửi; hủy thắng TRƯỚC khi gửi (giữa các attempt) thì không có reserve nào cho attempt đó, không ghi sổ gì. |
  | Provider disconnect (EOF/lỗi giữa chừng) | Không (hoặc một phần) | Như timeout: giữ reserve làm estimate, gắn cờ `provider_disconnect=true` + `outcome_unknown=true` (trừ khi response đã hoàn chỉnh trước khi disconnect, khi đó xử lý như response hoàn chỉnh). |
  | `UnknownOutcome` (lỗi body) | Tùy attempt provider đang bay | `UnknownOutcome` không tự giải phóng reserve. Attempt provider đang bay tại thời điểm lỗi body được settle theo đúng các hàng trên (có usage → measured; không → giữ reserve làm estimate). |

- **Ngưỡng:** hết ngân sách theo agent → chỉ agent đó dừng (lỗi `budget_exhausted`, audit);
  hết cap toàn cục → tất cả dừng (fail-all, cùng họ với G6-9). Ngân sách mặc định: theo
  agent = cap toàn cục / max_agents (chia đều, cấu hình được); cap toàn cục mặc định =
  **không giới hạn khi backend là loopback** (token local không tốn tiền — nhưng vẫn ghi sổ),
  và **bắt buộc cấu hình khi có backend trả phí** (kế thừa tinh thần cổng `CostCap`: không cấu
  hình thì backend trả phí đã bị khóa từ `build_provider`).
- **Hiển thị:** UI và tài liệu ghi rõ ngân sách/token là **ƯỚC TÍNH, không phải hóa đơn**;
  số token ghi sổ là dự trữ/ước tính, có thể khác thực tế nhà cung cấp tính. Cấm hiển thị
  USD có vẻ chính xác khi chỉ có reserve (xem §15.4, F6D-19). Cổng `cap_usd` hiện tại giữ
  nguyên vai trò opt-in, không bị ledger thay thế.
- Mọi bản ghi `provider_attempt` mang `agent_id` + `attempt_id` + token đã tính phí (kèm
  loại: measured/estimate) + số dư còn lại (theo agent + toàn cục).

### 3.4 Kiểm chứng (bản đồ)

- `g6_2_cross_agent_replay_refused`, `g6_3_stop_halts_all`, `g6_4_audit_attribution_under_load`,
  `g6_5_notebook_isolation`, `g6_6_atomic_global_cap`, `g6_7_cross_agent_snapshot_refused` —
  xem bảng cổng §11. Mỗi test đặt tên đúng cổng nó khóa.
- `snapshot_slot_reaped_with_agent` (F6D-09): agent kết thúc → slot của nó bị xóa;
  `take_for` sau đó thất bại; slot của agent còn sống không bị ảnh hưởng; TTL vẫn áp dụng.
- `notebook_concurrent_writes_no_lost_update` (F6D-10): 2 agent ghi đồng thời N dòng →
  mọi ghi thành công đều đọc lại được nguyên vẹn; sửa dòng shared của agent khác bị từ chối
  và dòng gốc không đổi.
- `ledger_settle_rules` (D-DESIGN-4): ma trận settle — response đủ usage / thiếu usage /
  timeout / cancel-sau-gửi / cancel-trước-gửi / disconnect / UnknownOutcome — mỗi ca assert
  đúng loại dữ liệu ghi sổ (measured/estimate), không giải phóng reserve khi outcome chưa
  biết, không double-count khi retry (attempt mới, settle cũ không đổi), settle lần hai bị
  từ chối.
- `experience_store_agent001_only_commit` (F6D-07): trajectory của agent-002+ không vào kho
  chung (quarantine) cho tới khi lượt cha được xác minh; agent-001 commit bình thường.

---

## 4. Quyết định G6-9: lỗi 1 agent thì sao?

### 4.1 QUYẾT ĐỊNH (một hướng, có ngoại lệ liệt kê) — GIỮ HƯỚNG, đồng thuận 3/3 (D-DESIGN-1)

> **Lỗi chạm tới BODY → FAIL-CLOSED CHO TẤT CẢ AGENT. Lỗi không chạm tới body → CHỈ AGENT ĐÓ
> dừng, các agent khác tiếp tục.**

Cụ thể:

- **Fail-closed tất cả** khi: body action timeout, body panic, actor thread chết/disconnect,
  hoặc bất kỳ lỗi nào khiến `BodyActor` bị đánh dấu `broken` — tức mọi trường hợp hôm nay đã
  báo `unknown_outcome` ở chế độ đơn agent. Khi đó: mọi agent đang chạy đều dừng; mọi action
  đang xếp hàng đều bị deny (không chạy muộn); audit ghi rõ cho cả hai phạm vi (agent gây lỗi
  + toàn cục); trạng thái stop latch được ghi rõ ràng (không mơ hồ); body stack chỉ chạy lại
  sau khi coordinator dựng lại actor mới (đường rebuild hiện có, không tự phục hồi âm thầm).
- **Chỉ agent đó** khi: lỗi provider (transient/permanent), guard deny, tool nội bộ lỗi,
  MCP server chết, `execute_code` lỗi/timeout trong sandbox, agent task panic (bắt ở
  `JoinHandle`, không kéo sập app), hết ngân sách theo agent. Agent đó kết thúc ở trạng thái
  `Failed` có audit đầy đủ; các agent khác không bị ảnh hưởng.
- **Ngoại lệ fail-all thứ hai (đã quyết ở §3.3):** hết cap toàn cục → tất cả dừng. Đây không
  phải "lỗi 1 agent" mà là giới hạn toàn cục, nên không mâu thuẫn với hướng chính.
- **Stop latch không bao giờ mơ hồ:** sau mọi lỗi fail-all, coordinator ghi 1 bản ghi
  `kind=agent_failure_scope` với `scope=all|single`, `cause`, `failed_agent`,
  `stop_latched=true|false`, `actor_broken=true|false`. Panic của 1 agent task KHÔNG trip
  stop latch (panic ≠ stop); chỉ lỗi body hoặc lệnh dừng của user mới trip.

**Ba điều kiện bổ sung của ChatGPT (D-DESIGN-1), bắt buộc:**

- (a) **Kiểm soát actor cũ sau khi dựng actor mới:** actor mới không được tạo cảm giác hành
  động cũ đã chắc chắn chấm dứt. Mọi action của actor cũ còn ở trạng thái chưa biết
  (`UnknownOutcome`) vẫn giữ nguyên trạng thái đó trong audit và UI sau khi rebuild; rebuild
  không viết lại lịch sử, không biến "chưa xác định" thành "đã dừng" hay "đã xong". Actor cũ
  (thread kẹt, nếu còn) không nhận action mới nào; chỉ actor mới phục vụ, và lượt đầu tiên
  sau rebuild phải chụp lại trạng thái màn hình mới trước khi hành động (không tin trạng
  thái cũ).
- (b) **Audit ghi rõ phạm vi + nguyên nhân + trạng thái xác minh kết thúc:** bản ghi
  `agent_failure_scope` thêm `end_verified=true|false` + `end_verification` (cách xác minh:
  `joined` | `timeout_unverified` | `not_applicable`) cho từng agent bị ảnh hưởng. Không có
  bản ghi nào được tuyên bố "đã dừng" khi kết thúc chưa được kiểm chứng (§10.1 đường 3).
- (c) **UI phân biệt 3 loại agent:** agent thất bại (gây lỗi) / agent bị dừng theo hệ thống
  (fail-all lan truyền) / kết quả chưa xác định (`UnknownOutcome`) — ba trạng thái hiển thị
  khác nhau, không gộp chung thành "lỗi" hay "đã dừng" (xem mô hình trạng thái §15.1,
  F6D-16).

### 4.2 Lý do

1. **Trạng thái màn hình sau lỗi body là không biết.** Một action timeout/panic có thể đã
   thực thi một phần (đây chính là ý nghĩa của `unknown_outcome`). Cho agent khác click tiếp
   lên một màn hình không biết trạng thái là lái mù — nguy hiểm hơn dừng tất cả.
2. **Kế thừa ngữ nghĩa đơn agent.** Latch `broken` hôm nay đã là vĩnh viễn-cho-mọi-request;
   quyết định này chỉ nói rõ điều đó áp dụng cho mọi agent, không phát minh hành vi mới.
3. **Lỗi không chạm body là độc lập thật.** Provider call của A không ảnh hưởng B; MCP server
   chết không ảnh hưởng body; sandbox lỗi không ảnh hưởng lượt khác. Fail-all ở đây chỉ tốn
   công vô ích và che mất nguyên nhân thật.
4. **Tiền lệ worker:** `catch_unwind` quanh lượt ở `worker_loop` đã chứng minh triết lý
   "lỗi 1 lượt không giết worker" — quyết định này mở rộng đúng triết lý đó, trừ đường body
   nơi trạng thái dùng chung là màn hình thật.

### 4.3 Điểm chạm code hiện có

- `BodyActor::is_broken`/`mark_broken`, `BodyError::UnknownOutcome` (`actor.rs`,
  `body/error.rs`): coordinator theo dõi `is_broken()` sau mỗi lỗi body; khi broken → phát
  lệnh dừng tất cả (qua stop token chung + hủy từng agent, §10) + ghi bản ghi scope.
- `actor_loop` panic path: đã bắt panic, báo `UnknownOutcome`, dừng loop — giữ nguyên; coordinator
  chỉ thêm phản ứng fail-all ở tầng trên.
- `WorkerEvent::WorkerFault` (`worker.rs`): mở rộng thành `AgentFault { agent_id, scope, cause }`
  để UI phân biệt lỗi 1 agent với lỗi toàn cục (khi cờ tắt, hiển thị như `WorkerFault` cũ).
  UI dùng thêm trường phạm vi/nguyên nhân/xác minh kết thúc để render 3 loại agent ở §15.1.

### 4.4 Kiểm chứng (fault injection, đúng như cổng G6-9 yêu cầu)

- `g6_9_body_hang_fails_all`: `SleepingBody` (tiền lệ test trong `actor.rs`) treo action của
  agent A → timeout → cả A và B đều dừng, không tool nào chạy thêm, audit nguyên vẹn, latch
  rõ ràng, hàng đợi còn lại bị deny.
- `g6_9_body_panic_fails_all`: `PanicBody` → kết quả như trên; app không sập.
- `g6_9_body_disconnect_fails_all`: actor thread chết giữa action → như trên.
- `g6_9_provider_failure_isolates_single`: provider của A lỗi vĩnh viễn → chỉ A `Failed`,
  B hoàn thành lượt.
- `g6_9_agent_task_panic_isolates_single`: task của A panic → coordinator ghi `AgentFault`
  scope=single, B tiếp tục, stop latch không trip, app sống.
- `g6_9_mcp_death_isolates_single`: fake MCP server chết giữa call của A → chỉ A nhận tool
  error, B không ảnh hưởng (xem §6).
- `g6_9_rebuild_does_not_rewrite_unknown` (D-DESIGN-1a): sau lỗi body + rebuild actor mới,
  audit vẫn ghi action cũ là `UnknownOutcome` (không bị sửa thành xong/dừng); lượt mới chụp
  lại màn hình trước khi hành động; actor cũ không nhận action mới.
- Mọi test G6-9 assert thêm: bản ghi `agent_failure_scope` có đủ `scope`/`cause`/
  `end_verified`/`end_verification` (D-DESIGN-1b), và UI nhận đủ dữ liệu để phân biệt 3 loại
  agent (D-DESIGN-1c, render kiểm chứng ở §15.6).

---

## 5. Sandbox `execute_code` theo OS

### 5.1 Linux — cơ chế ĐÃ KIỂM CHỨNG trên vega: bubblewrap (bộ cờ MỚI, F6D-01/F6D-03/F6D-04)

**Quyết định.** Trên Linux, `execute_code` chỉ chạy bên trong **bubblewrap** với đúng bộ cờ
đã chứng minh ở `04-sandbox-proof-v2.log`. Không có bwrap, hoặc preflight thất bại →
`execute_code` **từ chối fail-closed** kèm lý do `sandbox_unavailable`, ghi audit, không bao
giờ chạy code ngoài sandbox.

Bộ cờ (đúng như proof v2, theo đúng thứ tự đã chạy — thứ tự được đóng băng, không sắp xếp
lại nếu chưa chứng minh lại):

```
bwrap --unshare-all --die-with-parent --new-session \
  --dev /dev --proc /proc \
  --ro-bind /usr /usr --ro-bind /lib /lib --ro-bind /lib64 /lib64 \
  --ro-bind /bin /bin --ro-bind /etc /etc \
  --size 67108864 --tmpfs $HOME \
  --size 67108864 --tmpfs /tmp \
  --bind <workdir> /work --chdir /work \
  <interpreter> <script>
```

- `--unshare-all` bao gồm user + mount + net + pid + ipc + uts: proof cho thấy bên trong chỉ
  còn `lo` (0 packet), kết nối TCP ra ngoài lỗi `Network is unreachable`, trong khi baseline
  ngoài sandbox kết nối được — mạng bị chặn là thật, không phải do máy mất mạng. PID namespace
  được chứng minh phụ: shell trong sandbox mang PID 2 (PID 1 là tiến trình init/reaper của
  bwrap) trong log proof v2.
- `--new-session` (F6D-03): tạo session terminal mới (`setsid`), ngắt sandbox khỏi
  controlling terminal — code trong sandbox không inject input vào terminal của cha.
- `--tmpfs $HOME` với `--size 67108864`: proof cho thấy sentinel dưới HOME không đọc được
  (exit 1) và file ghi vào HOME không tồn tại ngoài sandbox — vùng HOME thật bị che hoàn
  toàn; mount ghi `size=65536k`.
- `--tmpfs /tmp` với `--size 67108864` (F6D-01): `/tmp` thật của host không nhìn thấy được
  trong sandbox — sentinel riêng trong `/tmp` ngoài không đọc được (exit 1), thư mục
  sentinel không tồn tại, `/tmp/.X11-unix` không tồn tại, `ls /tmp` trong sandbox trống
  (proof ca D). Đây là bản vá cho bộ cờ cũ (không che `/tmp`, socket X11 và file host trong
  `/tmp` đều lộ).
- `--size 67108864` (64 MiB) cho tmpfs (F6D-04): bwrap 0.12.0 **CÓ hỗ trợ** — `--help` ghi
  `--size BYTES: Set size of next argument (only for --tmpfs)`, man xác nhận `--size` chỉ
  tác dụng lên `--tmpfs` ngay sau nó và các mount sau không bị ảnh hưởng. Ép giới hạn đã
  chứng minh: ghi 80 MiB vào `/tmp` → dd exit 1 `No space left on device`, file dừng đúng
  67108864 byte (proof ca F). **Mỗi `--tmpfs` cần một `--size` đứng ngay trước nó** — quên
  `--size` thứ hai thì mount thứ hai về kích thước mặc định (theo man, đã kiểm chứng).
- `--bind <workdir> /work` + `--chdir /work` (F6D-01): workdir do parent tạo mới cho mỗi lần
  chạy, đặt **ngoài `/tmp`** (dưới `$HOME`), quyền **0700** (proof ghi `WORKDIR_MODE=700`,
  `WORKDIR_OUTSIDE_TMP=YES`), bind vào đường dẫn cố định `/work` trong sandbox, process bắt
  đầu tại `/work`. Code + input do parent ghi vào workdir TRƯỚC khi spawn; output đọc lại từ
  workdir sau khi tiến trình kết thúc. Bwrap tự tạo mountpoint `/work` (đã chứng minh: `/work`
  không tồn tại trên host nhưng bind vẫn thành công). Thứ tự cờ như trên đã chứng minh được:
  bind source dưới `$HOME` vẫn resolve được dù `--tmpfs $HOME` đứng trước — không đổi thứ tự
  khi chưa chứng minh lại.
- **Mạng luôn TẮT** trong sandbox Phase 6. Không có đường "cho phép mạng" nào: bắc cầu mạng
  (veth/bridge) cần đặc quyền/vận hành riêng, ngoài phạm vi, chưa kiểm chứng.
- **Không có socket hiển thị:** không bind `/tmp/.X11-unix`, không truyền `DISPLAY`/
  `WAYLAND_DISPLAY` — code trong sandbox không lái được GUI (ép luật R5, §7; cơ chế ép đầy
  đủ ở F6D-15 dưới đây).
- **Cơ chế ép R5 đầy đủ (F6D-15):** network namespace của `--unshare-all` khiến cả socket
  filesystem `/tmp/.X11-unix` (đã bị `--tmpfs /tmp` che) lẫn abstract socket X11 (sống trong
  network namespace, namespace mới không có socket của host) đều không tới được; cộng thêm
  không `DISPLAY`/`WAYLAND_DISPLAY`, không mạng. Đây là ép ở tầng cô lập, **không scan nội
  dung code** (scan không đáng tin — ghi rõ).
- **Cơ chế diệt tận gốc (F6D-03):** trên Linux, diệt tận gốc dựa vào **PID namespace +
  `--die-with-parent`**: mọi tiến trình con của code đều nằm trong cùng PID namespace; khi
  tiến trình init của namespace chết hoặc bwrap/cha chết, kernel kết liễu toàn bộ namespace
  (kèm SIGKILL từ `--die-with-parent` khi bwrap hoặc cha của bwrap chết) — không tiến trình
  mồ côi nào thoát ra ngoài namespace. Đây là cơ chế chính; giết process group (§10) là lớp
  thứ hai, không phải lớp duy nhất.
- **Giới hạn của setsid/kill(-pgid) ngoài PID namespace (F6D-03):** với tiến trình KHÔNG nằm
  trong PID namespace (trường hợp duy nhất ở Phase 6: MCP server unsandboxed theo ngoại lệ
  §6.3), `setsid` + `kill(-pgid)` chỉ là best-effort: tiến trình con tự gọi `setsid()` sẽ
  thoát khỏi group, tiến trình double-fork thành daemon sẽ thoát khỏi group, và không có cơ
  chế nào ở tầng group đảm bảo diệt tận gốc. Rủi ro mồ côi còn sót này được ghi rõ trong tài
  liệu vận hành + audit của ngoại lệ unsandboxed, và là một lý do ngoại lệ đó không bao giờ
  là chế độ mặc định.
- **FD/TTY — kỷ luật spawn bắt buộc (F6D-03):** spawn con qua `Command` với stdio cấu hình
  tường minh (stdin/stdout/stderr chỉ định rõ, không kế thừa fd ngoài stdio). **Bwrap KHÔNG
  tự đóng fd kế thừa** — đã chứng minh ở proof ca E2: cha giữ fd 9 mở trên file sentinel,
  con trong sandbox thấy `fd 9 -> <sentinel>` và **đọc được nội dung sentinel qua fd đó**
  (exit 0) dù tmpfs đã che HOME. Code triển khai KHÔNG được dựa vào bwrap để vệ sinh fd; mọi
  fd ngoài stdio phải đóng trước exec (fd do Rust `std` mở đều có CLOEXEC; cấm mở fd không
  CLOEXEC trên đường spawn). Kiểm chứng: test con liệt kê `/proc/self/fd` chỉ thấy stdio +
  fd dự kiến (proof ca E1: spawn sạch chỉ thấy 0/1/2 + fd 10 là fd file script của dash —
  fd dự kiến của harness; fd 3 xuất hiện thoáng qua trong vòng lặp liệt kê là pipe của chính
  phép thay thế lệnh dùng để liệt kê, không phải fd kế thừa).
- **Giới hạn tài nguyên sandbox (F6D-04):** ngoài `--size` tmpfs, ba RLIMIT được đặt qua
  `pre_exec` trước khi exec bwrap (cùng đường kế thừa như lớp bọc `prlimit` dùng trong
  proof — giới hạn đặt ngoài bwrap được con bên trong kế thừa):
  - `RLIMIT_FSIZE` — **đã chứng minh ép được**: prlimit 1 MiB → ghi 2 MiB vào `/work` chết
    ở đúng 1048576 byte (dd exit 153, SIGXFSZ). Giá trị mặc định cấu hình: 64 MiB (§12),
    bằng trần tmpfs; test triển khai phải kiểm chứng ép ở đúng giá trị cấu hình.
  - `RLIMIT_AS` — **đã chứng minh ép được**: giới hạn 256 MiB → cấp phát 128 MiB OK, cấp
    phát 512 MiB → `MemoryError`; giới hạn nhìn thấy trong `/proc/self/limits` của con.
    Giá trị mặc định cấu hình: 1 GiB (§12).
  - `RLIMIT_NPROC` — **KHÔNG đặt trước bwrap (kết quả âm đã chứng minh):** `NPROC=32` và
    `NPROC=200` đều làm bwrap chết ngay lúc tạo namespace (`Resource temporarily
    unavailable`) trong khi uid đã có ~182 tiến trình — việc đếm chạm tới tiến trình ngoài
    sandbox và bwrap tự fork nhiều tiến trình setup, nên mọi giá trị chặt đều mong manh và
    phá vỡ spawn. Phase 6 không đặt NPROC trước bwrap; chống fork-bomb dựa vào PID namespace
    + `--die-with-parent` + giết theo timeout + trần AS. Đặt NPROC bên trong namespace sau
    setup là hardening tương lai, chưa chứng minh, không thuộc Phase 6.
- **Ngôn ngữ cho phép:** `python3` và `sh` — đúng 2 interpreter đã chạy được trong rootfs
  tối thiểu của proof (python3 đã chạy bên trong). Ngôn ngữ khác → deny `unsupported_language`.
  Không compiler, không cài package trong sandbox (không mạng, `/usr` chỉ đọc).
- **Môi trường con:** tối thiểu — `PATH`, `HOME=<workdir>`, `TMPDIR=<workdir>`; KHÔNG truyền
  biến môi trường của tiến trình cha (đặc biệt không `MIMINT_API_KEY`, không key nào), trừ
  các biến khai báo tường minh trong cấu hình sandbox (mặc định: không có). (Lọc env là yêu
  cầu đối với code spawn; proof shell không kiểm chứng lọc env — test triển khai
  `execute_code_no_ambient_env` phải kiểm chứng.)
- **Preflight:** lúc khởi động (và trước lần chạy đầu tiên sau mỗi lần rebuild cấu hình),
  chạy kiểm tra với đúng bộ cờ §5.1: `bwrap --version` + 1 lần chạy thử thu nhỏ của proof
  (sentinel deny + allowed read + sentinel `/tmp` deny). Preflight hỏng → verdict
  `sandbox_unavailable` được cache, mọi `execute_code` từ chối kèm lý do, UI hiện rõ trạng
  thái "execute_code không khả dụng" thay vì im lặng (thông báo theo §15.4). Preflight không
  bao giờ tự hạ cấp xuống chạy không sandbox. Vì preflight chạy đúng bộ cờ đầy đủ, bwrap
  không hỗ trợ `--size`/`--new-session` sẽ tự làm preflight hỏng → fail-closed, không cần
  kiểm tra cờ riêng.
- **Không fallback sang `unshare` thuần.** Khả năng của `unshare` đã chứng minh được
  (U1–U4, `02-userns-probe.log`), nhưng proof 3 ca dùng bwrap. Để giữ đúng một đường code đã
  chứng minh đầu-cuối, Phase 6 chỉ nối bwrap; `unshare` thuần là cơ chế dự phòng cần proof
  riêng của nó trước khi nối. Ghi rõ để review không hiểu lầm "unshare đã chứng minh được thì
  dùng được ngay".
- **Seccomp:** kernel có `SECCOMP_FILTER=y` + `libseccomp` có mặt (đã kiểm), nhưng KHÔNG có
  filter nào được thử → Phase 6 **không dựa vào seccomp**, không tuyên bố có filter. Bổ sung
  filter (VD `--add-seccomp` của bwrap) là việc hardening riêng, cần proof riêng.
- **Landlock:** không thấy `CONFIG_LANDLOCK` trong config kernel → không dựa vào, không tuyên
  bố có.

**Luật chạy code (mọi OS, phần chung):**

- `execute_code` là tool `Consequential`: qua approver khi chạm file/mạng/hệ thống — thực tế
  là **mọi lần chạy đều qua approver** trong Phase 6 (code là hành động có hệ quả theo định
  nghĩa; không có ngoại lệ "code chỉ đọc"). Thẻ phê duyệt hiện: ngôn ngữ, hash code, workdir,
  timeout, và (khi có) các file input — nội dung thẻ đầy đủ ở §15.2 (F6D-17), gồm hash rút
  gọn có thể mở rộng + kiểm chứng được hash phê duyệt == hash thực thi.
- Timeout cứng: mặc định **30s**, cấu hình 5..300s. Hết timeout → giết process group (§10),
  kết quả `timeout`, không retry tự động.
- Giới hạn output: stdout+stderr gộp tối đa **1 MiB**; vượt thì cắt + gắn cờ `truncated`.
  Output trả về cho model là text đã cắt; binary output không được trả (ghi file vào workdir
  thì đọc lại qua đường khác, vẫn chịu giới hạn).
- Audit mỗi lần chạy: `code_sha256` (SHA-256 của code, tính bằng ring đã có), ngôn ngữ, độ
  dài code, `sandbox_verdict` (cơ chế + kết quả preflight), exit code, thời gian chạy, kết
  quả (ok/timeout/denied/error). **Không ghi text code vào audit** (tránh rò rỉ secret/code
  vào log). Workdir được giữ tới hết lượt để debug, rồi xóa; code không tồn tại ở đâu khác.
- Code hash + input file hash phải khớp giữa lúc phê duyệt và lúc chạy: args phê duyệt là
  immutable `Value` như mọi tool (tiền lệ guard) — không resolve lại.

### 5.2 macOS — CHƯA KIỂM CHỨNG, KHÔNG BỊA MỐC THỜI GIAN (F6D-22)

- Cơ chế đề xuất (chưa kiểm chứng, không tuyên bố có): `sandbox-exec` với profile tự viết
  (deny mặc định, chỉ cho đọc interpreter + workdir, deny network). Đây là hướng từng tồn tại
  trong nghiên cứu andee (`run_command` sandbox chỉ trên Mac, sandbox-exec + phân loại rủi
  ro) — chỉ là gợi ý tiền lệ, không phải bằng chứng.
- **Proof 3 ca bị chặn ở máy của user (mục BỊ CHẶN, xem cổng mở ở §13): cho tới khi proof
  chạy thật trên máy Mac của user và được ghi log, `execute_code` trên macOS từ chối
  fail-closed** với lý do `sandbox_unverified_on_macos`. Thiết kế này không quyết profile
  sandbox-exec cụ thể; người triển khai macOS phải viết profile + proof và trình review riêng.
  **Không có mốc thời gian nào được hứa cho việc này** — trạng thái "chưa kiểm chứng" không
  được để "mãi" bằng cách im lặng: cổng mở ở §13 ghi rõ điều kiện đóng cổng là log proof
  3 ca thật trên máy đó.
- Không có đường "chạy không sandbox trên Mac vì tin code" nào.

### 5.3 Windows — CHƯA KIỂM CHỨNG, KHÔNG BỊA MỐC THỜI GIAN (F6D-22)

- Cơ chế đề xuất (chưa kiểm chứng): Job Object (kill-on-close + giới hạn) kết hợp restricted
  token / AppContainer cho tiến trình con + ACL chỉ cho workdir. Giới hạn mạng cần spike riêng.
- **Proof 3 ca bị chặn ở máy của user (mục BỊ CHẶN, xem cổng mở ở §13): cho tới khi proof
  chạy thật trên Windows và được ghi log, `execute_code` trên Windows từ chối fail-closed**
  với lý do `sandbox_unverified_on_windows`. Thiết kế này không quyết chi tiết Job
  Object/AppContainer; cần spike + proof riêng trước khi nối. **Không có mốc thời gian nào
  được hứa cho việc này** — điều kiện đóng cổng ở §13.
- Không có đường chạy không sandbox nào.

### 5.4 Android — KHÔNG CUNG CẤP trong Phase 6 (quyết định, không phải hoãn)

- `execute_code` **không có trong registry** khi body là Android. Lượt yêu cầu nó → deny
  `unknown_tool` như mọi tool ngoài allow-list.
- Lý do: body Android là shell từ xa qua protocol v2; "sandbox trên điện thoại" không có
  nguyên mẫu đã kiểm chứng nào trong cây, và chạy code qua shell protocol đồng nghĩa thực thi
  trên thiết bị người dùng mà không có vùng cô lập đã chứng minh. Đây là quyết định phạm vi,
  không phải "sẽ làm sau" — muốn có thì cần thiết kế + proof riêng.

### 5.5 Điểm chạm code hiện có

- Module mới (đề xuất tên, chưa code): `crates/mimint-core/src/sandbox.rs` cho verdict +
  preflight + policy + workdir (tạo ngoài `/tmp`, 0700, dọn sau lượt) + đặt RLIMIT qua
  `pre_exec` + spawn với stdio tường minh, không kế thừa fd; tool `execute_code` đăng ký
  trong `Registry` như `BodyTool` (schema sinh từ implementation, §6.2). Tool `execute`
  đồng bộ: spawn `std::process::Command`, chờ có timeout trên thread gọi (cùng mô hình với
  `BodyTool` chờ actor).
- `StepVerifier` (§7, R4): output code được xác minh lại bằng GUI khi lượt có mặt giao diện.
- Audit: dùng `AuditLog` hiện có + kind `code_execution` mới (xem §5.1).

### 5.6 Kiểm chứng

- `sandbox_proof_on_vega`: test tích hợp chạy proof đầy đủ bằng chính code sandbox (không
  phải script shell) với đúng bộ cờ §5.1: sentinel HOME deny + network deny + allowed
  read/write `/work` + sentinel `/tmp` deny + `/tmp/.X11-unix` vắng mặt, trên máy có bwrap;
  bỏ qua (skip có ghi rõ) khi preflight hỏng — không bao giờ pass giả bằng cách chạy không
  sandbox.
- `sandbox_tmpfs_size_enforced` (F6D-04): ghi vượt 64 MiB vào tmpfs `/tmp` trong sandbox →
  thất bại, file dừng ở đúng giới hạn; mount ghi đúng `size=65536k` cho cả hai tmpfs.
- `sandbox_rlimits_enforced` (F6D-04): với giá trị cấu hình mặc định, FSIZE và AS nhìn thấy
  trong sandbox và ép được (ghi vượt FSIZE thất bại; cấp phát vượt AS thất bại). Test ghi rõ
  NPROC KHÔNG được đặt trước bwrap (kết quả âm ở `05-rlimit-scope.log`).
- `sandbox_child_fd_hygiene` (F6D-03): con liệt kê `/proc/self/fd` chỉ thấy stdio + fd dự
  kiến; cha giữ fd mở trên file nhạy cảm trong lúc spawn → con không thấy fd đó (bài học E2).
- `sandbox_preflight_uses_new_flags`: preflight chạy đúng bộ cờ §5.1 (gồm `--tmpfs /tmp`,
  `--size`, `--new-session`, workdir 0700 ngoài `/tmp`); bwrap thiếu cờ → preflight hỏng →
  fail-closed.
- `execute_code_refuses_without_sandbox`: giả lập bwrap vắng mặt (PATH rỗng / preflight ép
  hỏng) → mọi lần chạy deny `sandbox_unavailable`, không tiến trình con nào được spawn.
- `execute_code_timeout_kills_process_group`: code ngủ 120s + timeout 1s → tiến trình chết
  thật, không còn tiến trình con sau khi tool trả về (kiểm bằng bảng tiến trình con đã theo
  dõi + PID namespace đã đóng, §10).
- `execute_code_output_capped`: code in 5 MiB → output trả về ≤ 1 MiB + cờ `truncated`.
- `execute_code_audit_has_hash_not_text`: audit của 1 lần chạy chứa `code_sha256`, không chứa
  text code (kể cả khi code chứa chuỗi sentinel).
- `execute_code_no_ambient_env`: code in toàn bộ env nó thấy → không chứa `MIMINT_API_KEY`
  hay biến ambient nào ngoài danh sách tối thiểu (§5.1).

---

## 6. Hợp đồng MCP client

### 6.1 Quyết định vận chuyển + phiên bản protocol

- **Transport: STDIO DUY NHẤT trong Phase 6.** Client spawn server làm tiến trình con và trao
  đổi JSON-RPC 2.0 dạng NDJSON (mỗi message 1 dòng, không newline nhúng) qua stdin/stdout
  của con; stderr của con chỉ là log, không bao giờ parse thành message.
- **Giới hạn khung truyền (F6D-05):** giới hạn cứng **256 KiB mỗi raw frame NDJSON**, đọc qua
  `take()` **TRƯỚC khi cấp phát chuỗi** (đọc có giới hạn từ pipe, không `read_to_string`/
  `read_line` không giới hạn). Frame vượt giới hạn → lỗi protocol, server bị đánh dấu chết.
  Stderr của con được xả liên tục qua **ring-buffer giữ 16 KiB cuối** (hoặc `/dev/null`) —
  cấm đọc không giới hạn, cấm không xả (pipe stderr đầy sẽ deadlock server).
- **Streamable HTTP: KHÔNG triển khai.** HTTP+SSE: KHÔNG triển khai (đã deprecated ở phía
  spec). Lý do: stdio không mở socket nghe, không thêm bề mặt egress; HTTP cần thêm SSE parse,
  auth (OAuth), và nối `GuardedHttpClient` — mỗi thứ là một thiết kế riêng. Muốn HTTP thì cần
  thiết kế bổ sung + review riêng, không phải "thêm sau cho tiện".
- **Phiên bản protocol — giữ legacy cho Phase 6 + ADR bắt buộc (D-DESIGN-3):** client đề xuất
  `2025-06-18` trong `initialize`. Chấp nhận server trả về `2025-06-18` hoặc `2025-11-25`
  (cả hai đều thuộc thế hệ handshake, `tools/list` + `tools/call` ổn định, không batching).
  Mọi giá trị khác — gồm thế hệ stateless `2026-07-28` và mọi chuỗi lạ — đều bị từ chối,
  server không được nối, ghi audit. (Các revision của spec: 2024-11-05, 2025-03-26,
  2025-06-18, 2025-11-25, 2026-07-28.)
- **Sự thật về `2026-07-28` (đã kiểm chứng độc lập bằng tìm kiếm web ngày 18/11/2026, cả hai
  URL nguồn dưới đây đều mở được và đọc được cùng ngày):** `2026-07-28` là spec MCP hiện
  hành, stateless, bỏ handshake `initialize` và `Mcp-Session-Id`, mỗi request mang version +
  capability trong `_meta`, có `server/discover`; các tính năng cũ (handshake, sampling,
  roots, logging) còn được hỗ trợ tối thiểu 12 tháng (tới tháng 7/2027) theo chính sách
  deprecation. Nguồn:
  - Blog MCP 28/07/2026: <https://blog.modelcontextprotocol.io/posts/2026-07-28/>
  - Spec 2026-07-28: <https://modelcontextprotocol.io/specification/2026-07-28>
- **ADR bắt buộc:** quyết định giữ legacy được ghi trong `docs/adr/0001-mcp-legacy-protocol.md`
  (lý do chưa hỗ trợ 2026-07-28 + điều kiện mở rộng + mốc deprecation tháng 7/2027 làm động
  lực). Thiết kế này tham chiếu ADR đó; code không được mở rộng sang 2026-07-28 nếu chưa có
  ADR sửa đổi + review.
- **Kiểm tra compatibility + thông báo rõ:** khi gặp server chỉ hỗ trợ stateless
  (`2026-07-28`), client từ chối kèm thông báo rõ ràng cho user: "server yêu cầu phiên bản
  không được hỗ trợ" (kèm version server yêu cầu + version client hỗ trợ trong audit và hiển
  thị phụ). Không từ chối im lặng, không báo lỗi chung chung.
- **CẤM giả lập handshake:** cấm giả lập trao đổi `initialize`/`initialized` cũ với server
  chỉ-stateless (không gửi `tools/list` với version giả định, không tự "dịch" stateless
  thành handshake). Server không hoàn thành handshake legacy hợp lệ thì không có tool nào
  được đăng ký — ép bằng code (không có đường nào tới `tools/list` mà không qua `initialize`
  thành công), kiểm chứng bằng test + grep cổng (§6.6).
- **Bắt tay:** `initialize` (kèm `protocolVersion`, `capabilities: {}`, `clientInfo`) →
  server trả version + capabilities + `serverInfo` → client gửi `notifications/initialized`
  → `tools/list` (có phân trang, cộng dồn tối đa 100 tool/server). Client KHÔNG cung cấp
  capability nào: không roots, không sampling — server yêu cầu ngược đều bị từ chối (§6.4).
- **Không batching:** không bao giờ gửi mảng JSON-RPC; response dạng mảng → lỗi protocol,
  server bị đánh dấu chết. (Batching đã bị gỡ khỏi spec từ 2025-06-18.)

### 6.2 Discovery + gọi tool qua registry ACI (không tin schema server mù)

- Mỗi tool của server trở thành một `Tool` trong `Registry` hiện có, tên
  `mcp_<server>_<tool>` (ký tự chỉ `[a-z0-9_]`, sanitize + cắt 64 ký tự; trùng tên trong 1
  server hoặc giữa các server → từ chối server đó, không ghi đè âm thầm).
- **Schema sinh từ implementation, kẹp bởi client:** `inputSchema` của server phải qua cổng
  kẹp trước khi vào `parameters_schema()`:
  - phải là object JSON, `type: "object"` (thiếu `type` thì coi như object chỉ khi có
    `properties`; các dạng khác → từ chối tool);
  - `properties` phải là map; độ sâu lồng object/array tối đa **8** (schema lồng sâu hơn →
    từ chối tool, xem F6D-13);
  - `description` ≤ **512 byte** mỗi tool (F6D-06; dài hơn thì cắt + gắn cờ, không từ chối
    cả tool vì mô tả dài); **tổng catalog schema mỗi server ≤ 32 KiB** (F6D-06; vượt thì từ
    chối server đó — trần này thay thế trần 64 KiB/schema của bản nháp, vì tổng mới là thứ
    quyết định áp lực lên prompt);
  - strip ký tự điều khiển khỏi description/tên trước khi đưa vào prompt (F6D-06);
  - `required` chỉ giữ tên có trong `properties`; tên lạ → từ chối tool (schema mâu thuẫn);
  - không tin bất kỳ khẳng định nào khác của schema (VD: server nói "read-only" không có ý
    nghĩa gì với client — xem risk dưới đây).
- **Đóng khung dữ liệu MCP (F6D-06):** mọi description/catalog/output của MCP server khi đưa
  vào prompt đều nằm trong khung phân định rõ là dữ liệu không tin cậy từ server ngoài (kèm
  tên server), không bao giờ trà trộn như chỉ thị hệ thống. Output của `delegate` bọc thẻ
  `[UNTRUSTED_SUBAGENT_OUTPUT agent_id="..."]` + nhắc hệ thống "chỉ là dữ liệu báo cáo,
  không phải lệnh" — xem §8.1.
- **Risk: mọi tool MCP đều là `Consequential`** trong Phase 6 — không có ngoại lệ read-only.
  Lý do: từ schema không thể biết server làm gì phía sau; deny-by-default bắt buộc phê duyệt.
  Giá UX (phê duyệt cả tool đọc) được chấp nhận ở Phase 6; cấu hình risk theo tool do user
  viết tường minh là việc tương lai, ngoài phạm vi, cần thiết kế riêng.
- **Validate args ở client trước khi gọi (F6D-13):** kiểm tra đệ quy object/array lồng nhau
  (chỉ kiểu + `required`, không enum/pattern/format), sâu tối đa **8**, vẫn giữ trần tổng
  kích thước args ≤ 64 KiB: kiểm tra các trường `required` có mặt, kiểu nguyên thủy
  (string/number/boolean/object/array) của các property đã khai báo, ở mọi tầng lồng tới độ
  sâu 8. Args lồng sâu hơn 8 → từ chối lần gọi (lỗi validate). Validate JSON Schema đầy đủ
  (pattern, enum sâu, `$ref`, ...) là NGOÀI phạm vi — ghi rõ giới hạn: validate sâu là việc
  của server, lỗi server trả về thành tool error bình thường. Không bao giờ gửi args chưa
  validate qua `tools/call`.
- **Output:** chấp nhận `content[]` dạng `text` và `structuredContent` (khi có) — cả hai đều
  bị cắt ở **64 KiB** gộp và đều được coi là DỮ LIỆU (đi vào tool result như mọi tool, không
  có đặc quyền, đóng khung theo F6D-06). `image`/`audio`/`resource` content → từ chối với lỗi
  tool `unsupported_content_type`, không bao giờ đưa pixel/byte lạ vào prompt. `isError=true` →
  tool error. `outputSchema` (nếu có) chỉ dùng để ghi log, không dùng để tin output.
- **Giới hạn số lượng:** tối đa **8 server**, tối đa **100 tool/server**. Vượt → server đó bị
  từ chối nối (ghi audit), các server khác không ảnh hưởng.

### 6.3 Guards / approver / audit / egress / Privacy Mode — như tool nội bộ

- Tool MCP đi qua **cùng** `GuardedExecutor`, cùng `request_id`, cùng thẻ phê duyệt, cùng
  audit `decision`/`execution` như tool nội bộ. Thẻ hiện: tên tool, tên server, args immutable
  (như mọi thẻ), tên agent (§1) + nội dung thẻ đa agent ở §15.2 (F6D-17). Không có đường tắt
  nào.
- Audit thêm trường: `mcp_server`, `mcp_tool`, `protocol_version` (trong `detail` của bản ghi
  decision/execution; `serverInfo.name/version` ghi 1 lần lúc nối thành công, kind
  `mcp_server_connected`).
- **Egress / Privacy Mode — nói thật giới hạn:** stdio server là tiến trình con; Mimint
  không mở socket nào cho nó, nhưng tiến trình con tự nó có thể ra mạng (nó thừa hưởng môi
  trường + quyền của user). Đây là cùng giới hạn "chỉ đảm bảo cho tiến trình Mimint" đã ghi
  trong `docs/privacy-and-cost.md` cho backend loopback. Biện pháp giảm thiểu (quyết định):
  1. Server chỉ đến từ allow-list tường minh trong file cấu hình (`[[mcp-server]]`, §12).
     Không tự phát hiện, không server từ biến môi trường, không server do model yêu cầu thêm.
  2. Môi trường con bị lọc: chỉ `PATH`, `HOME`, và biến khai báo tường minh trong cấu hình
     server đó. Không bao giờ truyền secret ambient (đặc biệt không `MIMINT_API_KEY`).
  3. **Trên Linux: server chạy BÊN TRONG sandbox bwrap** (§5.1) khi sandbox khả dụng —
     không mạng, chỉ thấy workdir + rootfs chỉ đọc. Sandbox không khả dụng → server không
     khởi động (fail-closed, cùng preflight với `execute_code`).
  4. **Unsandboxed — ngoại lệ bảo mật có chủ đích, đã siết (D-DESIGN-2):**
     - Mặc định `unsandboxed = false` trên **MỌI OS**.
     - **Linux: `unsandboxed = true` BỊ TỪ CHỐI (lỗi cấu hình)** — bwrap đã chứng minh,
       không có lý do gì để chạy trần trên Linux.
     - **macOS/Windows: ngoại lệ opt-in từng server**, yêu cầu đủ: (a) xác nhận RIÊNG ở cấp
       server (không chỉ approval từng tool call — xác nhận gắn với server, ghi audit, trước
       lần khởi động đầu tiên của server đó); (b) cảnh báo liệt kê quyền truy cập
       filesystem/network/tài nguyên hệ thống mà server sẽ có; (c) chỉ báo "đang chạy
       unsandboxed" hiển thị **liên tục** trong lúc server chạy; (d) audit việc bật ngoại lệ
       (server + thời điểm + ai xác nhận); (e) **KHÔNG BAO GIỜ tự chuyển sang unsandboxed
       khi sandbox thất bại** — sandbox thất bại thì server không chạy, không hạ cấp âm thầm.
     - **Android: không MCP, không ngoại lệ.**
     - Phân biệt threat model vẫn được giữ (binary tĩnh do user cấu hình ≠ code do model
       sinh ra trong lượt), nhưng không dùng nó để biện minh cho chế độ hoạt động bình
       thường không sandbox — ngoại lệ chỉ tồn tại vì sandbox macOS/Windows chưa kiểm chứng
       (§5.2/§5.3), và sẽ thu hẹp khi proof có.
  5. Trên Android: không có MCP server (không spawn tiến trình con qua shell protocol).
  6. Khi Privacy Mode ON: vẫn cho chạy server (chúng là tiến trình local), nhưng lúc khởi động
     in cảnh báo liệt kê từng server + nhắc giới hạn egress; quyết định này được ghi vào
     `docs/privacy-and-cost.md` khi triển khai (sửa docs là việc của giai đoạn 2, không phải
     bản thiết kế này).
  7. **File cấu hình chứa `[[mcp-server]]` phải nằm ngoài tầm ghi của mọi tool và ngoài mọi
     `<workdir>`** (D-DESIGN-2): đường dẫn cấu hình không bao giờ nằm trong workdir sandbox
     hay trong vùng tool ghi file được chạm tới; kiểm chứng bằng test (tool ghi file không
     chạm được đường dẫn cấu hình). Model không bao giờ tự thêm server bằng cách sửa file
     cấu hình.
- **Integrity — giới hạn đã chấp nhận (F6D-21):** Phase 6 **không xác minh hash/nguồn gốc
  binary MCP server** — chỉ kiểm tra đường dẫn tuyệt đối + allow-list + quyền file. Binary
  bị thay thế sau khi cấu hình sẽ chạy mà không bị phát hiện ở tầng này. **Hash pinning là
  việc tương lai, không phải Phase 6.** Giới hạn này phải ghi trong tài liệu vận hành; không
  tuyên bố MCP server đã được xác minh nguồn gốc.

### 6.4 Timeout + hủy + server chết

- Timeout mỗi lần gọi: mặc định **30s**, cấu hình 5..300s (chung thang với `execute_code`).
  Hết timeout → gửi `notifications/cancelled` (best-effort) rồi **giết process group** (§10),
  server bị đánh dấu chết, lần gọi lỗi `timeout`. Server chạy trong bwrap được diệt tận gốc
  bằng PID namespace + `--die-with-parent` (§5.1); server unsandboxed (ngoại lệ §6.3) chỉ
  được diệt best-effort bằng process group, với giới hạn đã ghi ở §5.1.
- Hủy lượt/stop: cùng đường hủy level-3 (§10) — tiến trình con bị giết thật, không chỉ cờ.
- Server chết (EOF stdout, exit, output không parse được, response sai id/kiểu, frame vượt
  256 KiB): mọi lần gọi đang chờ đều lỗi; server bị đánh dấu chết; **không kéo sập app**
  (I/O con chạy trên thread riêng, mọi parse đều fallible, không `unwrap` trên output của
  server); **không restart tự động trong cùng lượt** — lượt sau muốn gọi lại thì thử khởi
  động lại tối đa 1 lần, hỏng thì server nghỉ hết lượt đó. Chết của server là lỗi chỉ-agent-đó
  theo G6-9 (§4).
- Server gửi request ngược: `sampling/createMessage` → **JSON-RPC Error `-32601 Method not
  found` hợp lệ** (F6D-08 — trả lỗi đúng định dạng, không im lặng, để không treo state machine
  của SDK chuẩn lành tính); `roots/list` → `-32601` như trên; `ping` → trả pong. Notification
  lạ → bỏ qua + đếm (không crash, không log nội dung).
- Server gửi `notifications/tools/list_changed` → client **không** tự nạp lại tool giữa lượt;
  thay đổi chỉ áp dụng ở lần nối tiếp theo (giữa các lượt). Lý do: registry của lượt đang chạy
  là bất biến (tiền lệ ACI: surface của lượt được chụp lúc bắt đầu).

### 6.5 Điểm chạm code hiện có

- Module mới (đề xuất, chưa code): `crates/mimint-core/src/mcp.rs` (client JSON-RPC + cổng
  kẹp schema + đọc frame có giới hạn + ring-buffer stderr) hoặc crate con nếu review yêu cầu
  tách; quyết định vị trí là việc của review, bản này chỉ yêu cầu: không dependency mới, dùng
  `std::process::Command` + `serde_json` + `AuditLog` hiện có. Spawn server dùng cùng kỷ luật
  stdio/fd như sandbox (§5.1, F6D-03).
- `Registry`/`Tool` (`tools.rs`): tool MCP implement đúng trait hiện có; `parameters_schema()`
  trả schema đã kẹp; `risk()` luôn `Consequential`; `execute()` đồng bộ (gửi qua channel tới
  thread I/O của server, chờ có timeout — cùng mô hình `BodyTool` chờ actor).
- `GuardedExecutor`, `UiApprover`, `AuditLog`: không đổi chữ ký ngoài `agent_id` (§1.3);
  tool MCP tự đi qua đúng đường. Xác nhận cấp server + chỉ báo unsandboxed liên tục + audit
  bật ngoại lệ là trách nhiệm của tầng UI/worker (§15, D-DESIGN-2).
- Cấu hình: `[[mcp-server]]` trong `FileConfig` (§12), `deny_unknown_fields`; validate
  `unsandboxed` theo OS (Linux → lỗi cấu hình).

### 6.6 Kiểm chứng (server giả tự viết, không phụ thuộc server thật)

- Fake server là binary test trong cây (test fixture), implement đúng subset protocol +
  các chế độ lỗi: treo, rác output, output khổng lồ, exit giữa call, version sai, batch
  response, yêu cầu sampling, frame vượt giới hạn, stderr ồ ạt. Không test nào chạm mạng hay
  server ngoài.
- `mcp_version_negotiation_matrix`: echo `2025-06-18` → nối; echo `2025-11-25` → nối; echo
  `2026-07-28`/`2024-11-05`/chuỗi lạ → từ chối, không tool nào đăng ký; ca `2026-07-28` assert
  thông báo "server yêu cầu phiên bản không được hỗ trợ" + audit ghi version (D-DESIGN-3).
- `mcp_no_faked_handshake`: fake server chỉ-stateless (không trả `initialize` hợp lệ) →
  client không gửi `tools/list`, không tool nào đăng ký; grep cổng: không đường code nào tới
  `tools/list` mà không qua `initialize` thành công (D-DESIGN-3).
- `mcp_schema_clamp`: schema quá khổ / thiếu properties / required lạ / tên trùng / lồng sâu
  hơn 8 → tool hoặc server bị từ chối đúng như §6.2; schema hợp lệ → schema vào model đúng
  bản đã kẹp.
- `mcp_catalog_caps` (F6D-06): description >512 byte → cắt + cờ; tổng catalog >32 KiB/server
  → server bị từ chối; ký tự điều khiển bị strip; dữ liệu MCP trong prompt nằm trong khung
  phân định.
- `mcp_recursive_args_validation` (F6D-13): args lồng nhau sai kiểu/thiếu required ở tầng
  sâu → từ chối trước khi gửi; args lồng sâu hơn 8 → từ chối; args hợp lệ sâu ≤8 → gửi được.
- `mcp_frame_cap_256k` (F6D-05): fake server gửi frame >256 KiB → client lỗi protocol, server
  bị đánh dấu chết, không cấp phát không giới hạn (đọc qua `take()`).
- `mcp_stderr_ring_bounded` (F6D-05): fake server xả stderr ồ ạt → client không deadlock,
  bộ nhớ có chặn trên, chỉ giữ 16 KiB cuối.
- `mcp_all_tools_consequential`: mọi tool MCP đều qua approver; không approver → deny.
- `mcp_timeout_kills_server`: fake server treo → sau timeout không còn tiến trình con, lần
  gọi lỗi `timeout`, app sống, lượt khác không ảnh hưởng.
- `mcp_server_death_isolates_agent`: server chết giữa call → chỉ agent đó nhận tool error.
- `mcp_output_caps_and_types`: output 1 MiB → cắt 64 KiB; content image → lỗi
  `unsupported_content_type`; `isError` → tool error.
- `mcp_sampling_rejected`: server gửi `sampling/createMessage` → client trả lỗi `-32601`
  đúng định dạng JSON-RPC, không gọi provider nào (F6D-08); `roots/list` tương tự; `ping` →
  pong.
- `mcp_no_ambient_secrets`: server giả in toàn bộ env nó thấy → không chứa `MIMINT_API_KEY`
  hay secret ambient nào (test đặt sentinel env rồi kiểm tra vắng mặt).
- `mcp_unsandboxed_linux_refused` (D-DESIGN-2): cấu hình `unsandboxed = true` trên Linux →
  lỗi cấu hình, server không khởi động.
- `mcp_unsandboxed_requires_server_confirmation` (D-DESIGN-2): macOS/Windows — thiếu xác
  nhận cấp server → server không chạy dù `unsandboxed = true`; sandbox thất bại không tự
  chuyển sang unsandboxed; chỉ báo + audit bật ngoại lệ có mặt.
- `mcp_config_file_out_of_tool_reach` (D-DESIGN-2): tool ghi file không chạm được đường dẫn
  file cấu hình chứa `[[mcp-server]]`; đường dẫn cấu hình không nằm trong workdir nào.

---

## 7. Luật định tuyến GUI/Code CỨNG (Agent S số 4)

### 7.1 Quyết định: luật cứng = guard ép bằng code, không phải gợi ý trong prompt

Model được thấy cả hai bề mặt (tool body + `execute_code`), nhưng các bất biến dưới đây do
CODE ép: vi phạm thì bị từ chối + ghi audit, không phải "mong model làm đúng". Nơi nào không
ép được bằng code thì bản này nói rõ (không tuyên bố ép được).

| # | Luật | Ép bằng gì |
|---|---|---|
| R1 | Tính toán thuần / biến đổi dữ liệu / thao tác hàng loạt lên file / xử lý text không cần nhìn → CODE (`execute_code`). | Prompt hệ thống + thứ tự tool + guard R6. Không có classifier ép R1 (ghi rõ giới hạn). |
| R2 | Cần nhìn màn hình, phán đoán bố cục/trạng thái visual, hoặc tương tác phần tử GUI → GUI (tool `body_*`). | Prompt + R5 (code không chạm được GUI). |
| R3 | Tác vụ hỗn hợp: tách bước — bước tính → code, bước nhìn/chạm → GUI. Code chạy trước khi GUI xác minh (xem R4). | Prompt + R4. |
| R4 | **Output code PHẢI xác minh lại bằng GUI trước khi coi là xong, khi lượt có mặt giao diện.** "Mặt giao diện" = lượt có body stack gắn vào. Không có body stack (CLI/headless) → xác minh bằng đọc lại artifact (hash/nội dung file vừa ghi). Trạng thái xác minh 3 mức + phương thức (§7.2, F6D-14). | Guard trong `StepVerifier` (§7.2) + trạng thái xác minh + UI hiện badge khi chưa xác minh. ÉP ĐƯỢC. |
| R5 | **Code không bao giờ lái GUI.** Không synthetic input, không script UI (xdotool/pyautogui/AppleScript-UI/...) từ trong `execute_code`. | Sandbox: net namespace + `--tmpfs /tmp` che socket X11 + không biến `DISPLAY`/`WAYLAND_DISPLAY` + không mạng (§5.1, F6D-15). ÉP ĐƯỢC ở tầng cô lập. Không scan nội dung code (không đáng tin — ghi rõ). |
| R6 | **GUI không làm việc hàng loạt.** Quá 5 action body giống hệt nhau liên tiếp trong 1 lượt → lượt kết thúc với lỗi định tuyến `gui_batch_loop` + audit, kèm gợi ý dùng code. Ngưỡng cấu hình được, mặc định 5. "Giống hệt" = fingerprint + ngoại lệ cuộn (§7.1 dưới bảng, D-DESIGN-6). | Guard mới trong vòng lặp `Brain` (đếm theo lượt). ÉP ĐƯỢC. Không có đường user-vượt-guard trong lượt (D-DESIGN-6). |
| R7 | Không đường nào được rò rỉ secret: code không mạng (sandbox), GUI qua guard hiện có; code không đọc được file ngoài workdir (§5). | Sandbox + guard hiện có. |
| R8 | Khi mơ hồ → GUI (nhìn thấy được, phê duyệt từng bước). Mọi lần chọn đường đầu tiên của lượt + mọi lần đổi đường đều ghi audit `kind=routing` với `rule` + lý do. | Prompt (mặc định) + audit bắt buộc trong `Brain` khi tool đầu tiên của lượt chạy và khi họ tool đổi giữa `body_*` ↔ `execute_code`. |

**Định nghĩa fingerprint R6 (D-DESIGN-6):**

- "Giống hệt" = trùng cặp `(tool_name, canonicalized_json_args)`, trong đó
  `canonicalized_json_args` là JSON args ở dạng chuẩn tắc: key object sắp xếp, không khoảng
  trắng thừa, số so sánh theo giá trị số (không theo chuỗi). Hai lần gọi chỉ khác thứ tự key
  hoặc định dạng số tương đương vẫn tính là trùng.
- Với click/move: tọa độ lệch trong bán kính **Δ ≤ 2px** vẫn coi là trùng một điểm (chống
  lách guard bằng cách nhích 1px). Điểm so sánh là tâm điểm đã chuẩn tắc theo lưới 2px
  (tọa độ được lượng tử hóa trước khi so sánh, để nhích 1px qua lại không tạo hai điểm khác
  nhau vô hạn).
- **Ngoại lệ cuộn — chọn MỘT cơ chế: hash frame.** `body_scroll` /
  `body_key(PageDown|Down|Up)` chỉ tính là lặp khi **hash frame sau mỗi lần cuộn KHÔNG đổi**
  (đã chạm đáy mà vẫn cuộn). Hash frame = SHA-256 của byte frame chụp sau action (cùng hạ
  tầng hash đã có). Cơ chế ngưỡng riêng 12 lần đã được cân nhắc và LOẠI: ngưỡng 12 vẫn dương
  tính giả trên tài liệu rất dài. Khi không có frame mới sau action (không body stack / chụp
  thất bại), ngoại lệ hash không áp dụng và cuộn được đếm theo fingerprint thường (thận
  trọng: đếm, không bỏ qua).
- Ngưỡng mặc định 5, cấu hình được (`gui_batch_threshold`, §12). Đếm theo lượt, reset khi
  fingerprint đổi. Ngưỡng N nghĩa là N action giống hệt được chạy, action thứ N+1 kích hoạt
  guard: lượt kết thúc với `gui_batch_loop` + audit + gợi ý dùng code. Audit ghi đủ thông tin
  giải thích vì sao guard kích hoạt (fingerprint, số lần đếm, ngưỡng, so sánh hash frame khi
  là ca cuộn). UI giải thích đây là bảo vệ chống lặp thao tác, không phải lỗi hệ thống.
- **BÁC đề xuất "cho user chọn tiếp tục/tắt khóa tạm thời" (D-DESIGN-6):** R6 là guard an
  toàn, không có đường tiếp tục hay tắt tạm thời trong cùng lượt. Lý do: một vòng lặp GUI
  thật có thể click/xóa hàng loạt trong lúc user còn đang đọc thẻ; đường vượt trong lượt sẽ
  biến guard thành gợi ý. User muốn thử lại thì chạy lượt mới (bộ đếm mới, ngữ cảnh mới).

### 7.2 Cơ chế xác minh R4 (F6D-14)

- `StepVerifier` hiện có (`brain.rs`, `BodyStepVerifier` ở `verify.rs`) được mở rộng: khi tool
  vừa chạy là `execute_code` và lượt có body stack, verifier chụp screenshot mới qua actor và
  ghi frame meta vào bản ghi verification (giống hệt cơ chế đã có cho tool body). Verifier
  vẫn chỉ quan sát (ghi "đã đọc lại màn hình"), không phán xét đạt mục tiêu — phán xét thuộc
  về model đọc frame mới và checker của golden task (tiền lệ `verify.rs`).
- Khi lượt không có body stack: verifier đọc lại artifact — với `execute_code`, đó là re-hash
  các file output trong workdir (so với hash ghi lúc tool trả về) + kiểm tra file tồn tại.
  Không đọc lại được → verification `failed`.
- **Trạng thái xác minh (F6D-14):** mỗi bước/lượt mang một trạng thái xác minh:
  `Verified(method)` với `method = gui | artifact`, hoặc một trong 3 mức chưa-đạt:
  **chưa xác minh** (`Unverified` — chưa chạy xác minh), **xác minh thất bại** (`Failed` —
  xác minh đã chạy và thất bại), **xác minh không khả dụng** (`Unavailable` — không thể xác
  minh trong ngữ cảnh này: không body stack VÀ không có artifact để đọc lại). UI badge hiển
  thị phân biệt cả 2 phương thức verified lẫn 3 mức chưa-đạt (5 cách hiển thị, xem §15.5); `TurnOutcome.all_verified = true` chỉ khi mọi
  bước cần xác minh đều `Verified`. `WorkerEvent::TurnDone` thêm trường `verified` + phương
  thức; UI hiện badge tương ứng, không im lặng báo xong.
- **R4 fallback re-hash là giới hạn đã biết (F6D-14):** re-hash chỉ bảo chứng hành vi — file
  vẫn tồn tại với đúng byte đã ghi — không phải bảo chứng mật mã cho tính đúng đắn của kết
  quả, và không chứng minh code đã làm đúng việc. Tài liệu và UI không được tuyên bố quá:
  badge cho `Verified(artifact)` ghi rõ "đã đọc lại artifact", khác với "đã xác minh bằng
  giao diện".
- Hệ quả: lượt có mặt giao diện mà có bước code chưa qua GUI verify thì `TurnOutcome.all_verified
  = false`. Không có đường nào biến "chưa xác minh" thành "xong" mà không qua verify.

### 7.3 Điểm chạm code hiện có

- `brain.rs`: vòng lặp (đếm R6, audit `routing`), `verify_previous` (mở rộng cho
  `execute_code`), `TurnOutcome.all_verified` (đã có, nay mang thêm ý nghĩa R4 + trạng thái
  xác minh §7.2).
- `StepVerifier` trait + `BodyStepVerifier` (`verify.rs`): thêm nhánh `execute_code` (khi có
  body stack) và nhánh artifact re-read (khi không có); trả về trạng thái xác minh §7.2.
- `docs/`: luật ghi vào file mới `docs/gui-code-routing.md` (việc của giai đoạn 2) + nhắc
  trong bản thiết kế này. Bản thiết kế này là nguồn của file đó.
- Sandbox (§5): ép R5/R7 ở tầng cô lập.

### 7.4 Kiểm chứng (test khóa luật)

- `r4_code_output_requires_gui_verification`: lượt có body stack, `execute_code` ghi file →
  verification chụp screenshot mới (frame id mới trong audit); ép verifier hỏng → bước
  `failed`, `all_verified=false`, UI nhận `verified=false`.
- `r4_headless_artifact_reread`: lượt không body stack → verification bằng re-hash; xóa file
  output trước khi verify → `failed`; trạng thái `Verified(artifact)` phân biệt với
  `Verified(gui)` (F6D-14).
- `r4_verification_status_levels` (F6D-14): 3 mức chưa-đạt + 2 phương thức verified đều có
  thể đạt được và hiển thị phân biệt; `Unavailable` khi không body stack và không artifact.
- `r5_code_cannot_reach_display`: trong sandbox, `DISPLAY`/`WAYLAND_DISPLAY` vắng mặt và
  `/tmp/.X11-unix` không tồn tại/đọc được (kiểm bằng code chạy trong sandbox thật trên vega);
  abstract socket X11 không tới được (kết nối tới socket X11 thất bại).
- `r6_gui_batch_loop_guard`: model script gọi 6 lần `body_click` giống hệt → lượt dừng ở lần
  thứ 6 với `gui_batch_loop` + audit; 5 lần thì vẫn chạy; click nhích 1px vẫn tính là trùng
  (Δ ≤ 2px); audit ghi đủ fingerprint + số đếm + ngưỡng (D-DESIGN-6).
- `r6_scroll_hash_exception` (D-DESIGN-6): cuộn tài liệu dài mà frame đổi mỗi lần → không
  kích hoạt guard; cuộn khi frame không đổi (chạm đáy) → đếm lặp và kích hoạt ở ngưỡng.
- `r6_false_positive_form_fill`, `r6_false_positive_long_scroll`,
  `r6_false_positive_repeated_confirm` (D-DESIGN-6, BẮT BUỘC trước khi đóng cổng): tác vụ GUI
  hợp lệ — điền 6 ô form, cuộn tài liệu dài, xác nhận lặp — không kích hoạt guard.
- `r6_no_in_turn_override` (D-DESIGN-6): không tồn tại đường code cho phép tiếp tục/tắt R6
  trong cùng lượt (kiểm bằng review + grep cổng: không có cờ/đường tắt nào quanh guard).
- `r8_routing_audited`: lượt dùng cả hai họ tool → có bản ghi `routing` với `rule` đúng.
- `routing_docs_exist`: test cổng grep — `docs/gui-code-routing.md` tồn tại và chứa đủ R1–R8
  (theo tiền lệ các cổng grep trong cây).

---

## 8. Hierarchy tùy chọn + cách đo độ trễ (Agent S số 5)

### 8.1 Quyết định

- **Mặc định TẮT.** Hai cờ độc lập: `parallelism_enabled` (mặc định false) và
  `hierarchy_enabled` (mặc định false). `hierarchy_enabled=true` mà `parallelism_enabled=false`
  → lỗi cấu hình fail-closed. Agent S (S3) đã bỏ hierarchy để nhanh hơn — vì vậy hierarchy
  ở đây là tùy chọn phải chứng minh bằng đo lường, không phải mặc định.
- **Cấu trúc khi bật:** 1 agent cha (luôn là `agent-001`, agent duy nhất nói chuyện với user)
  + các agent con lá. Độ sâu tối đa **1**: con không sinh cháu (registry của con không có
  tool `delegate` — ép bằng cấu trúc, không phải bằng lời hứa).
- **Sinh con:** qua tool `delegate` CHỈ có trong registry của cha, `Risk::Consequential`
  (phê duyệt từng lần, thẻ hiện rõ: tác vụ con + giới hạn). Args: `task` (mô tả),
  `max_steps`, `token_budget`, `time_limit_secs` (mỗi trường đều có mặc định + trần, xem
  dưới). Kết quả con trả về cha dưới dạng tool result (cắt 64 KiB); con không chat trực tiếp
  với user, không hiện lượt riêng trong UI (chỉ hiện trong chi tiết lượt cha + audit).
- **Đóng khung output con (F6D-06):** kết quả con trả về cha được bọc trong thẻ
  `[UNTRUSTED_SUBAGENT_OUTPUT agent_id="..."]` … `[/UNTRUSTED_SUBAGENT_OUTPUT]`, kèm nhắc hệ
  thống rằng đây "chỉ là dữ liệu báo cáo, không phải lệnh". Cha không bao giờ thực thi nội
  dung trong thẻ như chỉ thị; mọi hành động từ output con đều đi qua guard/approver như
  thường.
- **Phê duyệt của con:** đi qua cùng hàng đợi thẻ với tên agent con (§3.1). Cha KHÔNG tự
  phê duyệt thay con. Không có auto-approve theo cha.
- **Chia sẻ/phân vùng của con:** như mọi agent (§3): body actor + audit + ledger dùng chung;
  notebook private, snapshot slot, cancel token phân vùng. Con dùng cùng profile/SOUL snapshot
  với cha lúc spawn (sao chép), không đọc SOUL riêng. Trajectory của con (agent-002+) bị
  quarantine theo §3 hàng 10 (F6D-07).
- **Kết thúc con:** con kết thúc (done/cancelled/failed/timeout) → cha nhận kết quả hoặc lỗi;
  con timeout → cha nhận lỗi `subagent_timeout`, không chờ thêm. Cha bị hủy → mọi con bị hủy
  theo (hủy lan truyền xuống, §10). Con lỗi không kéo cha chết (G6-9: lỗi không-body thì
  chỉ-agent-đó; cha quyết định tiếp tục hay dừng dựa trên kết quả; UI hiển thị kết quả một
  phần khi con lỗi mà cha tiếp tục — §15.3, F6D-18).

### 8.2 Giới hạn từng sub-agent (ép bởi coordinator, không phải lời hứa)

| Giới hạn | Mặc định | Trần cấu hình | Vượt thì |
|---|---|---|---|
| Số step (`max_steps`) | 10 | 1..50 | Lượt con kết thúc `partial`, như lượt thường hết budget |
| Token budget | 1/4 ngân sách còn lại của cha lúc spawn | ≤ ngân sách còn lại của cha, và ≤ cap toàn cục còn lại | Con dừng `budget_exhausted`, cha nhận lỗi |
| Thời gian tường (wall time) | 300s | 60..1800s | Coordinator hủy level-3 (§10), cha nhận `subagent_timeout` |
| Số con đồng thời của 1 cha | 2 | 1..4 (và ≤ max_agents − 1) | `delegate` deny `subagent_limit` |

### 8.3 Cách đo độ trễ (số đo, baseline, ngưỡng tách chỉ tiêu — D-DESIGN-5)

- **Số đo (lấy từ audit + bản ghi thực thi, không thêm instrumentation ngoài 1 trường):**
  (a) wall time cả lượt (cha + con) từ bản ghi `turn_start`/`turn_end` theo `ts_ms`;
  (b) time-to-first-action (từ bắt đầu lượt tới execution đầu tiên);
  (c) thời gian chờ hàng đợi body mỗi action (`queued_ms` — trường mới trong bản ghi execution,
  §9); (d) tổng token tiêu thụ (ledger, §3.3).
- **Baseline:** cùng golden task, cùng model Ollama local, cùng máy, chạy ở chế độ đơn agent
  (cờ tắt) — tối thiểu 3 lần chạy, lấy median + khoảng. So sánh trên cùng bộ task: 1 bộ
  song song hóa được (2 việc độc lập) + 1 bộ không song song hóa được (việc nối tiếp).
- **Ngưỡng chấp nhận — tách chỉ tiêu (D-DESIGN-5):**
  - **Overhead điều phối thuần của Mimint ≤ 10%**, đo bằng stub provider nhanh (hoặc trừ thời
    gian provider thuần ra khỏi wall time). Đây là chỉ tiêu duy nhất đo phần Mimint kiểm soát
    được.
  - **Wall-time end-to-end với Ollama thật trên CPU 2 nhân ≤ 35–40%** (nới từ 25% của bản
    nháp, vì chi phí 2 lượt LLM điều phối của cha là bất khả kháng khi mọi agent cùng nghẽn
    trên 1 worker suy luận CPU); **token ≤ 2×**; **tác vụ nối tiếp ≤ 10%**. Giữ nguyên các
    số này.
  - Docs ghi rõ: lợi ích giảm wall-time thực sự chỉ đạt được khi các agent không cùng nghẽn
    trên một worker suy luận CPU duy nhất; trên Ollama local, giá trị của hierarchy là cô lập
    ngữ cảnh, không phải tốc độ (xem thêm F6D-20 ở §9.1).
- **Đạt ngưỡng KHÔNG tự bật hierarchy mặc định.** Vượt ngưỡng → hierarchy ở lại trạng thái
  thử nghiệm, mặc định tắt, và docs ghi rõ kết quả đo thật. **KHÔNG tự động sửa cấu hình của
  user** — chế độ vốn đã mặc định tắt; vượt ngưỡng chỉ nghĩa là không được quảng bá/bật mặc
  định, không phải một hành động sửa config âm thầm nào.
- **Báo cáo đủ:** 4 số đo (wall, time-to-first-action, queued, token) + số lần chạy + median
  + ca thất bại. ≥3 lần chạy là tối thiểu; **không tuyên bố độ tin cậy cao từ 3 lần chạy** —
  báo cáo ghi rõ cỡ mẫu và khoảng dao động.
- **Harness:** dùng lại golden task + eval harness hiện có (`mimint-core/eval`,
  `mimint-body/src/bin/evalharness.rs`, `mimint-ui/src/bin/golden.rs`) — chạy cùng task với
  cờ bật/tắt, không viết harness mới. Chạy trên vega với Ollama thật, không tải model mới,
  không API trả phí (theo CRITERIA P6.6/G2).

### 8.4 Điểm chạm code hiện có

- `Registry` theo agent (§2.1): `delegate` chỉ đăng ký cho cha. Tool `delegate` gọi
  coordinator (qua handle dùng chung, không qua body actor).
- `Brain` + `GuardedExecutor`: con là `Brain` bình thường với `agent_id` riêng, `max_steps`
  riêng, token riêng; không đổi vòng lặp.
- Coordinator (§2.4): ép giới hạn §8.2, lan truyền hủy cha→con, thu thập kết quả con, bọc
  output con theo F6D-06 trước khi trả cho cha.

### 8.5 Kiểm chứng

- `hierarchy_default_off`: cấu hình mặc định → không có tool `delegate`, spawn con bị từ chối.
- `hierarchy_depth_one`: con không có `delegate` trong schema; gọi `delegate` từ con → deny
  `unknown_tool`.
- `subagent_limits_enforced`: con vượt step/token/time → dừng đúng giới hạn, cha nhận đúng lỗi.
- `parent_cancel_cancels_children`: hủy cha giữa lúc con chạy → con dừng, không task con nào
  còn sống sau khi cha kết thúc (kiểm chứng join, §10).
- `delegate_output_framed` (F6D-06): kết quả con tới cha luôn nằm trong thẻ
  `[UNTRUSTED_SUBAGENT_OUTPUT agent_id="..."]` kèm nhắc hệ thống; nội dung trong thẻ không
  được thực thi như chỉ thị.
- `hierarchy_latency_measured`: chạy harness §8.3, báo cáo đủ 4 số + số lần chạy + median +
  khoảng + ca thất bại, kết luận đạt/không đạt từng ngưỡng tách chỉ tiêu — kết quả là bằng
  chứng vào `phase6/`, không phải test pass/fail tự động duy nhất (test tự động khóa phần đo
  được: `queued_ms` có mặt, token được ghi; ngưỡng là phán quyết của review trên số đo; đạt
  ngưỡng không tự bật mặc định).

---

## 9. Hàng đợi + SLA

### 9.1 Quyết định

- **Hàng đợi:** actor giữ hàng đợi con theo agent (§2.1). Sức chứa: **64 action đang chờ toàn
  cục, 16 action mỗi agent**. Enqueue vượt sức chứa → deny ngay `body_queue_full` + audit
  (không tăng vô hạn, không drop âm thầm). Kênh intake mpsc giữ nguyên không chặn; sức chứa
  và SLA được ép bên trong vòng lặp actor (vòng lặp không bao giờ chặn khi gửi reply — kênh
  reply không chặn, tiền lệ hiện tại).
- **Ghi chú thực tế (F6D-20):** trần 16/agent + 64 toàn cục là **trần dự phòng**, không phải
  độ sâu vận hành kỳ vọng — thực tế mỗi agent tối đa **1 action + 1 probe** trong hàng đợi,
  vì `Brain` chạy nối tiếp từng tool (mỗi agent chỉ có một lần gọi tool đang chờ tại một
  thời điểm, cộng tối đa một probe do approver phát ra cho agent đó). Hàng đợi chỉ đầy khi
  actor tắc kéo dài; trần tồn tại để chặn tăng vô hạn, không phải để chứa tải bình thường.
- **SLA chờ hàng đợi:** **60s** mặc định (cấu hình 10..300s). Request quá hạn mà chưa bắt đầu
  chạy thì **hết hạn an toàn**: bị deny, không bao giờ chạy muộn mù. Lý do: 1 click lên màn
  hình đã đổi sau 60s chờ là click sai; từ chối + để agent chụp lại màn hình mới là đúng.
  (Cấu hình `queue_sla_secs` phải được mô tả là thời gian chờ tối đa, không phải thời gian
  thực thi — §12, F6D-19.)
- **Cơ chế hết hạn:** mỗi request mang `enqueue_time` + `deadline`. Actor kiểm tra hết hạn ở
  2 điểm: khi quét hàng đợi mỗi vòng lặp (reap) và ngay trước khi dequeue để chạy. Hết hạn →
  reply deny + audit `kind=decision, decision=deny, reason=queue_sla_expired` với `queued_ms`
  và `agent_id`. Hết hạn hàng đợi KHÔNG làm hỏng actor (khác với timeout lúc đang chạy —
  request chưa bắt đầu thì không có `unknown_outcome`).
- **Timeout lúc chạy:** giữ nguyên **30s** mặc định (`DEFAULT_ACTION_TIMEOUT`), vẫn làm hỏng
  actor + `UnknownOutcome` như hôm nay (đang chạy thì không biết đã thực thi hay chưa).
- **Chờ reply ở phía caller:** `act()` chờ tối đa `queue_sla + action_timeout` (mặc định
  60+30=90s) — một deadline duy nhất bao cả chờ hàng đợi lẫn chạy. Caller timeout mà không
  nhận được reply vẫn hiểu là "có thể đang chạy" → đánh dấu broken (ngữ nghĩa hiện tại giữ
  nguyên). Actor-side deny (hết hạn/broken/stop) luôn trả reply nhanh nên caller không chờ
  thừa. Khi agent bị hủy, caller đang chặn được giải phóng tức thì bằng đẩy hủy (§10, F6D-02),
  không chờ hết deadline này.
- **Probe SLA:** **5s** (probe không bao giờ được làm chậm thẻ — nối tiếp thực hành "short
  bound" hiện tại của approver). Probe hết hạn → không có snapshot, thẻ vẫn hiện, re-verify
  bị bỏ qua cho action đó (ngữ nghĩa hiện tại).
- **Approval SLA:** giữ nguyên **60s** (`DEFAULT_APPROVAL_TIMEOUT`). Độ sâu hàng đợi thẻ ≤ số
  agent sống theo cấu trúc (§3.1) nên không cần cap riêng.
- **Số liệu:** mọi execution record thêm `queued_ms` (thời gian chờ hàng đợi, 0 khi chạy ngay).
  Audit cho phép đếm số lần vượt SLA theo agent. Không thêm UI đo lường trong Phase 6 (chỉ
  thị độ sâu hàng đợi là việc tương lai, ghi nhận, không làm).
- **Chi phí ẩn trên máy yếu (F6D-20):** song song chỉ có ích cho thời gian chờ provider khi
  provider là API cloud; trên Ollama local CPU, các agent xếp hàng/chia sẻ 1 worker suy luận
  (xem §8.3). Các chi phí ẩn phải tính khi đọc số đo độ trễ: fsync/SQLite (audit + notebook),
  cold-start bwrap+python3 (~40–120ms mỗi lần chạy `execute_code`), chi phí screenshot cho
  xác minh R4. Không chi phí nào trong số này được "tối ưu" trong Phase 6 — chúng được ghi
  nhận để số đo không bị đọc sai.

### 9.2 Điểm chạm code hiện có

- `actor.rs`: `Request` thêm `agent_id`/`enqueue_time`/`deadline`; `actor_loop` thêm hàng đợi
  con + round-robin + reap hết hạn; `BodyActor::act`/`probe_target_with_timeout` dùng deadline
  gộp. `DEFAULT_ACTION_TIMEOUT`, `broken`, `UnknownOutcome`: giữ nguyên ý nghĩa.
- `AuditLog`: thêm trường `queued_ms` vào detail của execution/decision (tương thích ngược:
  parser bỏ qua trường lạ).
- Cấu hình: `queue_sla_secs`, `max_queued_total`, `max_queued_per_agent` trong `[agents]`
  (§12), validate khoảng, sai thì lỗi cấu hình.

### 9.3 Kiểm chứng

- `queue_sla_expiry_denies_safely`: lấp đầy hàng đợi bằng action chậm của A, action của B chờ
  quá SLA → B nhận deny `queue_sla_expired`, action không chạy (oracle không thấy event),
  audit có bản ghi, actor không broken.
- `queue_capacity_denies_overflow`: vượt 16/agent hoặc 64 toàn cục → deny `body_queue_full`
  ngay, không treo, không mất request cũ.
- `queued_ms_recorded`: action chờ >0 có `queued_ms` > 0 trong audit; action chạy ngay có
  `queued_ms` = 0.
- `probe_sla_short`: probe chậm 6s → caller nhận lỗi sau ≤5s + margin, actor không broken,
  action tiếp theo vẫn chạy.
- `queue_depth_backstop_reality` (F6D-20): với `Brain` nối tiếp, mỗi agent không bao giờ có
  quá 1 action + 1 probe trong hàng đợi trong vận hành bình thường (assert trên harness mô
  phỏng lượt thật); trần 16/64 chỉ đạt được khi ép actor tắc.

---

## 10. Cancel level-3 (hủy thật)

Trạng thái hôm nay (đã đọc): mọi stop đều là level 2 (hợp tác) — `docs/stop-and-approval.md`
ghi rõ level 3 "NOT implemented", `CallContext` cố tình không có token hủy, HTTP client không
có abort. Phase 6 triển khai level 3 cho đúng 3 đường dưới đây — và nói rõ 1 đường KHÔNG làm
được.

### 10.1 Quyết định: 3 đường giết thật + đẩy hủy (F6D-02)

**Đẩy hủy — bắt buộc cho cả 3 đường (F6D-02):** khi hủy 1 agent, coordinator PHẢI đẩy tín
hiệu rút/hủy ngay vào `BodyActor` + `UiApprover` để trả `Err(Cancelled)` tức thì về channel
đồng bộ: request body đang chờ của agent đó được reply hủy trong vài ms (không chờ hết SLA),
thẻ phê duyệt đang chờ của agent đó được rút ngay (không chờ hết timeout thẻ). `CancelToken`
được truyền xuống vòng chờ tiến trình con (wait_timeout/poll ngắn) để vòng chờ thoát ngay khi
hủy thắng, thay vì ngủ hết timeout. Cam kết lượt kết thúc ≤5s chỉ được tuyên bố sau khi đo
thật ở cổng P6.4 (§10.4) — trước đó nó là mục tiêu, không phải khẳng định.

**Đường 1 — Provider call đang mở: drop future + ngắt kết nối.**

- `Brain::run` bọc mỗi lần gọi `provider.chat(...)` trong `tokio::select!` với 1 future hủy
  theo dõi token của agent (turn token + stop token). Hủy thắng → future chat bị drop (reqwest
  hủy request khi future bị drop, kết nối đóng), lượt kết thúc `Cancelled`, không chờ hết
  timeout 180s của chain.
- `FallbackChain` cũng kiểm tra hủy giữa các attempt (không bắt đầu attempt mới sau khi hủy
  thắng cuộc đua) — cả hai lớp cùng ép.
- Cơ chế chờ hủy: `CancelToken` thêm phương thức chờ async dựa trên `tokio::sync::Notify`
  (cần bật thêm feature `sync` + `time` của tokio — cùng crate đã có, 0 package mới, ghi rõ
  trong license audit của giai đoạn 2). Không dùng poll 50ms trong đường async nóng; cầu
  `StopBridge` 50ms hiện tại (body → token) giữ nguyên vì nó ở ranh giới thread.
- Mục tiêu đo được: hủy giữa lượt Ollama chậm → lượt kết thúc trong **5s** kể từ lúc hủy
  (test §10.4 đo thật; con số là mục tiêu, kết quả đo ghi vào bằng chứng; chỉ tuyên bố đạt
  sau khi đo — F6D-02).
- **Quy tắc dự phòng drop-future (F6D-12):** cổng P6.4 phải đo thời gian đóng kết nối thật
  trên **3 điều kiện mạng** (bình thường / chậm / mất kết nối) + xác minh không rò rỉ kết nối
  treo. **Nếu đo thất bại** (kết nối không đóng kịp, hoặc rò rỉ kết nối treo), triển khai hủy
  HTTP tường minh thay vì chỉ dựa vào drop, **và phải quay lại review trước khi tuyên bố
  cancel level-3 đạt**. Không có đường nào tuyên bố đạt level-3 trên giả định drop-future
  chưa đo.

**Đường 2 — Tiến trình con (`execute_code`, MCP server): giết process group thật.**

- Mọi tiến trình con do Mimint spawn đều chạy trong process group riêng (`setsid` trên
  Unix; `CREATE_NEW_PROCESS_GROUP` trên Windows — phần Windows chưa kiểm chứng, §5.3). Trên
  Linux, tiến trình trong bwrap còn được diệt tận gốc bằng PID namespace +
  `--die-with-parent` (§5.1) — lớp namespace là chính, group-kill là phụ. Ngoài PID namespace
  (MCP unsandboxed), group-kill chỉ là best-effort với giới hạn đã ghi ở §5.1.
- Hủy/timeout → SIGTERM cả group → chờ tối đa 2s → SIGKILL cả group nếu còn sống → `waitpid`
  thu hồi (không zombie). Verdict giết (`killed_term`/`killed_kill`/`already_exited` + thời
  gian) ghi vào audit của lần chạy. Vòng chờ dùng `CancelToken` + wait_timeout/poll ngắn
  (F6D-02) để phản ứng ngay khi hủy thắng.
- Coordinator theo dõi mọi tiến trình con đang sống theo agent; hủy agent → giết toàn bộ con
  của agent đó; kết thúc tiến trình chính → không còn con mồ côi (kiểm bằng test).
- Không có "cooperative flag" nào cho tiến trình con: cờ chỉ dùng để quyết định CÓ giết hay
  không, còn giết là giết thật ở tầng OS.

**Đường 3 — Task/thread agent: abort + join có kiểm chứng.**

- Hủy agent = hủy token của nó + `JoinHandle::abort()` task tokio của nó + await handle tối
  đa **5s**. Task kết thúc trong bound → trạng thái `Cancelled` đã kiểm chứng. Quá bound →
  audit `task_end_unverified` + coordinator chuyển sang trạng thái degraded (không spawn mới)
  cho tới khi user khởi động lại — không tuyên bố "đã dừng" khi chưa chứng minh được.
- Panic trong task → `JoinHandle` trả lỗi, coordinator ghi `AgentFault` scope=single (§4),
  các task khác không ảnh hưởng. Tiền lệ: `catch_unwind` quanh lượt trong `worker_loop`.

**Đường KHÔNG làm được (ghi rõ, không tuyên bố — nhấn mạnh trong tài liệu vận hành):**
thread của body actor KHÔNG BAO GIỜ bị giết — tài liệu `actor.rs` đã ghi: không có cách an
toàn để giết thread kẹt trong OS call nước ngoài; thread kẹt thì detach, actor bị đánh dấu
broken, caller dựng actor mới. Level-3 không thay đổi điều này: hủy chỉ ngăn action MỚI bắt
đầu (qua `StopFlag` + token), không giật thread body đang kẹt. **Đây là giới hạn có chủ đích,
phải ghi trong docs người dùng, tài liệu vận hành, và thông báo lỗi UI (§15.4)** — user phải
hiểu "DỪNG TẤT CẢ" không giật được thread body đang kẹt trong OS call, và trạng thái sau đó
là "kết quả chưa xác định" chứ không phải "đã dừng sạch" nếu actor còn kẹt.

### 10.2 Ngữ cảnh hủy trong audit

Mọi lần hủy ghi `kind=turn_end, decision=cancelled` với detail mở rộng: `agent_id`, `source`
(`ui_stop` | `turn_cancel` | `timeout` | `queue_sla` | `parent` | `coordinator` |
`budget_exhausted`), `in_flight` (`provider` | `tool` | `approval` | `none`),
`killed` (danh sách: `provider_call` | `child_process:<pid>` | `none`),
`cancel_latency_ms` (từ lúc hủy tới lúc lượt kết thúc), `push_cancelled` (đẩy hủy đã tới
actor/approver hay chưa — F6D-02). Các lý do cũ (`cancelled_before_step`,
`cancelled_between_tools`, `cancelled_during_tool`, `cancelled_after_approval`) giữ nguyên và
thêm các trường mới — không đổi chuỗi lý do cũ.

### 10.3 Điểm chạm code hiện có

- `brain.rs`: bọc `provider.chat` trong `select!`; kiểm tra hủy hiện có (đầu bước/giữa tool)
  giữ nguyên, thêm đường ngắt giữa chừng.
- `cancel.rs`: `CancelToken` thêm waiter async (`Notify`). Ngữ nghĩa latch/không un-cancel
  giữ nguyên.
- `fallback.rs`: kiểm tra hủy giữa attempt (nhận handle hủy qua `CallContext` mở rộng —
  `CallContext` thêm trường hủy tùy chọn; các caller cũ truyền `None` thì hành vi như cũ).
- `egress.rs`: không đổi — abort đạt được bằng drop future, không cần sửa HTTP client.
  (Giả định này phải đo thật trên 3 điều kiện mạng ở P6.4; nếu đo thất bại thì quay lại
  review + hủy HTTP tường minh theo quy tắc dự phòng F6D-12 — ghi rõ giả định này để test
  §10.4 kiểm chứng.)
- `worker.rs`: coordinator giữ `JoinHandle` + bảng tiến trình con; `TokenSlot` theo agent
  (map) thay cho 1 slot lượt duy nhất; stop slot vẫn chung. Coordinator sở hữu đường đẩy hủy
  vào actor + approver (F6D-02).
- `actor.rs` / `approver.rs`: thêm đường nhận tín hiệu rút/hủy: actor reply `Err(Cancelled)`
  cho request đang chờ của agent bị hủy; approver rút thẻ đang chờ của agent bị hủy (deny +
  audit), cả hai trong vài ms (F6D-02).
- `docs/stop-and-approval.md`: cập nhật bảng 3 level ở giai đoạn 2 (level 3: IMPLEMENTED cho
  provider + tiến trình con; actor thread: vẫn không giết được — ghi rõ + nhấn mạnh trong
  tài liệu vận hành).

### 10.4 Kiểm chứng

- `level3_cancel_slow_provider`: provider stub treo 180s + hủy → lượt kết thúc trong 5s,
  audit có `cancel_latency_ms` + `killed=[provider_call]`, không tool nào chạy sau hủy.
- `level3_push_cancel_releases_blocked_caller` (F6D-02): caller đang chặn trong
  `tool.execute()` chờ actor/approver → hủy agent → caller nhận `Err(Cancelled)` trong vài
  ms, không chờ hết SLA/timeout thẻ; audit có `push_cancelled=true`.
- `level3_approver_card_withdrawn_on_cancel` (F6D-02): thẻ đang chờ của agent bị hủy được rút
  ngay (deny + audit), thẻ của agent khác không bị ảnh hưởng.
- `level3_drop_future_measured_3_conditions` (F6D-12): đo thời gian đóng kết nối thật trên 3
  điều kiện mạng (bình thường/chậm/mất kết nối) + xác minh không rò rỉ kết nối treo; thất bại
  → kích hoạt quy tắc dự phòng (hủy HTTP tường minh + quay lại review), không tuyên bố đạt.
- `level3_cancel_kills_child_processes`: `execute_code` ngủ 120s + hủy → process group chết,
  không còn tiến trình con (kiểm bằng danh sách con đã theo dõi + `waitpid` đã thu hồi +
  PID namespace đã đóng trên Linux), audit có verdict giết.
- `level3_thread_end_verified`: sau hủy, mọi `JoinHandle` đều done; test assert không còn
  task agent nào sống (đếm qua coordinator).
- `level3_cancel_context_audited`: mọi đường hủy đều có đủ trường §10.2; thiếu trường → test
  đỏ.
- `level3_no_new_tools_after_cancel`: hủy giữa step có nhiều tool call → các tool còn lại
  không chạy (mở rộng bất biến hiện tại sang đường ngắt giữa chừng).

---

## 11. Đối chiếu cổng G6-1..G6-10 (cập nhật theo mọi thay đổi — F6D-24)

| Cổng | Mục thiết kế | Pass test sẽ viết (giai đoạn 2) |
|---|---|---|
| G6-1 Body serialization | §2 (round-robin + 1 actor), §9 | `g6_1_round_robin_no_interleaving` (oracle, MockBody): action nguyên vẹn, không xen event; `scheduler_starvation_bound` |
| G6-2 Approval isolation | §1 (id), §3.1 (phân vùng theo khóa + slot theo agent + dọn dẹp khi reap, F6D-09) | `g6_2_cross_agent_replay_refused`: decision của A đưa cho executor của B → `approval_id_mismatch`; thẻ mỗi agent ghi tên agent đó; thẻ trả sau khi agent bị hủy → deny; `snapshot_slot_reaped_with_agent` |
| G6-3 Stop semantics | §3 (stop latch chung), §10 (hủy lan truyền + đẩy hủy, F6D-02) | `g6_3_stop_halts_all`: trip giữa lúc 2 agent chạy → cả 2 dừng, không tool nào thêm, audit có stop cho cả 2 phạm vi; cancel riêng 1 agent chỉ dừng agent đó; `TripInfo` ghi agent gây trip; `level3_push_cancel_releases_blocked_caller` |
| G6-4 Audit attribution + integrity | §1.3 (`agent_id` first-class + tương thích schema, D-DESIGN-7), §3 hàng 4 | `g6_4_audit_attribution_under_load`: tải đồng thời N agent → file parse trọn JSONL, mọi dòng có `agent_id`, thứ tự trong lượt của mỗi agent đúng; viewer lọc theo agent; `audit_legacy_records_readable` (fixture JSONL cũ) |
| G6-5 Notebook isolation | §3.2 (owner/shared + migration + chống lost update đã có, F6D-10) | `g6_5_notebook_isolation`: 2 agent lượt đồng thời → không thấy private của nhau, không sửa được dòng của nhau, không lost update trên dòng shared; DB cũ migrate đúng; `notebook_concurrent_writes_no_lost_update` |
| G6-6 Cost accounting | §3.3 (ledger mới, reserve/settle nguyên tử + semantics đầy đủ, D-DESIGN-4) | `g6_6_atomic_global_cap`: N spender đồng thời không vượt cap toàn cục; hết budget agent → chỉ agent đó dừng; hết cap toàn cục → tất cả dừng; mọi `provider_attempt` có `agent_id` + token + loại dữ liệu; `ledger_settle_rules` (ma trận timeout/cancel/disconnect/UnknownOutcome, không double-count) |
| G6-7 Observation freshness per-agent | §3 hàng 8 (frame + slot theo agent, attribution + TTL, F6D-07), §3.1 (dọn slot, F6D-09) | `g6_7_cross_agent_snapshot_refused`: B không chạy được point action bằng snapshot của A; TTL tính trên frame mới nhất của chính agent hành động; frame store ghi `agent_id`; `snapshot_slot_reaped_with_agent` |
| G6-8 No silent parallelism | §1.4, §2.5, §8.1 (cờ mặc định tắt), §0.2 (ngoại lệ duy nhất + tương thích schema, D-DESIGN-7) | Full suite cũ với cờ tắt = baseline (trừ dòng thời gian theo tiền lệ conformance + trường `agent_id` trong file audit, §0.2); UI không hiện nhãn agent mới; `audit_legacy_records_readable` |
| G6-9 Failure isolation | §4 (QUYẾT ĐỊNH: lỗi body → tất cả; lỗi khác → 1 agent) + 3 điều kiện D-DESIGN-1 | 6 test fault-injection ở §4.4 (hang/panic/disconnect → tất cả; provider panic/task/MCP → 1 agent) + `g6_9_rebuild_does_not_rewrite_unknown` + assert audit `end_verified`/`end_verification` |
| G6-10 Design review before code | Bản này + vòng review §14 | Bản thiết kế đã review + sign-off ghi trong doc TRƯỚC commit code Phase 6 đầu tiên; doc cuối nằm ở `docs/phase6-design.md` trong repo; đủ F6D-01..F6D-24 + proof chạy lại + bảng sign-off (§14) |

Cổng chung G1–G4 (CRITERIA P6.6) không thuộc bản thiết kế này ngoài ghi nhận: G1 (test xanh +
clippy + fmt trên clone mới, coordinator đối chiếu độc lập), G2 (demo end-to-end qua Ollama
thật, ảnh chụp parent xem bằng mắt), G3 (license audit 0 forbidden + README/assets không đổi
+ 0 secret), G4 (bằng chứng vào `phase6/`, mục BỊ CHẶN vẫn ghi BỊ CHẶN). Mọi pass test trên
đều chạy được trên vega không sudo, không GPU, không mạng ngoài (trừ demo G2 dùng Ollama
local). Toàn bộ test của các cơ chế (§5.6, §6.6, §7.4, §8.5, §9.3, §10.4, §15.6) giữ tên như
đã nêu trong từng mục; bảng này chỉ khóa các test theo cổng G6.

---

## 12. Bề mặt cấu hình mới (phác thảo, giai đoạn 2 mới viết code parse)

`FileConfig` dùng `deny_unknown_fields` nên mọi section dưới đây phải khai báo tường minh.
Giá trị mặc định = hành vi hôm nay (mọi thứ tắt/khóa). Các trần an toàn (frame MCP 256 KiB,
ring stderr 16 KiB, catalog 32 KiB, description 512 byte, schema sâu 8, output code 1 MiB)
là **hằng số trong code, không cấu hình được** — user không thể nới lỏng chúng qua config.

```toml
[agents]
parallelism_enabled = false   # G6-8: mặc định TẮT
max_agents = 3                # 1..8; UI phải giải thích: thêm agent không đảm bảo tăng tốc (§12 dưới, F6D-19)
queue_sla_secs = 60           # 10..300; thời gian CHỜ tối đa, không phải thời gian thực thi (F6D-19)
max_queued_total = 64
max_queued_per_agent = 16
action_timeout_secs = 30      # giữ mặc định hiện tại của actor

[hierarchy]
enabled = false               # mặc định TẮT; true đòi parallelism_enabled=true
max_children = 2              # 1..4
default_child_steps = 10      # 1..50
default_child_time_secs = 300 # 60..1800

[sandbox]
# Linux: bwrap là cơ chế duy nhất; các OS khác: xem §5 (fail-closed khi chưa kiểm chứng)
execute_code_timeout_secs = 30  # 5..300
max_output_bytes = 1048576      # 1 MiB
rlimit_fsize_bytes = 67108864   # 64 MiB; đặt qua pre_exec trước bwrap (§5.1, F6D-04)
rlimit_as_bytes = 1073741824    # 1 GiB; đặt qua pre_exec trước bwrap (§5.1, F6D-04)
# KHÔNG có rlimit_nproc: NPROC chặt trước bwrap phá vỡ tạo namespace (đã chứng minh, §5.1)

[routing]
gui_batch_threshold = 5       # 2..50; ngưỡng guard R6 (§7.1, D-DESIGN-6)

[[mcp-server]]
name = "ten-server"           # [a-z0-9_], duy nhất
command = "/duong/dan/tuyet/doi/toi/server"  # đường dẫn tuyệt đối, không tìm PATH mù
args = []
env = {}                      # chỉ các biến khai báo tường minh mới được truyền
timeout_secs = 30             # 5..300
unsandboxed = false           # Mặc định false MỌI OS. Linux: true = LỖI CẤU HÌNH. macOS/Windows:
                              # true chỉ là một nửa của ngoại lệ — còn cần xác nhận riêng cấp
                              # server + cảnh báo + chỉ báo liên tục + audit (§6.3, D-DESIGN-2).
                              # Không bao giờ tự fallback sang unsandboxed khi sandbox hỏng.
```

Quy tắc validate (fail-closed, lỗi cấu hình rõ ràng): tên server trùng → lỗi; `command` tương
đối → lỗi; `hierarchy.enabled` mà không bật parallelism → lỗi; `unsandboxed = true` trên
Linux → lỗi; mọi số ngoài khoảng → lỗi; section lạ → lỗi (do `deny_unknown_fields`). Không có
server MCP hay sandbox nào được bật bằng biến môi trường. File cấu hình chứa `[[mcp-server]]`
phải nằm ngoài tầm ghi của mọi tool và ngoài mọi workdir (§6.3, D-DESIGN-2).

**Bề mặt cấu hình phân 3 tầng (F6D-19):** file TOML vẫn phẳng như trên, nhưng UI/tài liệu
cấu hình phải tổ chức thành 3 tầng — **cơ bản** (bật/tắt song song, số agent, SLA),
**nâng cao** (hierarchy, sandbox timeout, MCP server), **chuyên gia** (rlimit, trần hàng đợi,
ngưỡng R6). Quy tắc trình bày bắt buộc: `unsandboxed` phải mô tả quyền truy cập mà server sẽ
có (filesystem/network/tài nguyên hệ thống), không chỉ là checkbox; `max_agents` giải thích
rõ thêm agent không đảm bảo tăng tốc (đặc biệt trên Ollama local CPU — §8.3/§9.1);
`queue_sla_secs` nói rõ là thời gian chờ tối đa, không phải thời gian thực thi. Tầng chuyên
gia không được giấu các cảnh báo an toàn của tầng dưới.

---

## 13. Mục còn mở / BỊ CHẶN — thiết kế này KHÔNG quyết thay

Các mục dưới đây giữ nguyên trạng thái BỊ CHẶN / chưa quyết như CRITERIA P6.6/G4. Bản thiết
kế này không đưa ra quyết định kỹ thuật nào thay cho chúng, không tuyên bố chúng đã có, và
không code nào của Phase 6 được phép phụ thuộc vào chúng:

- **CỔNG MỞ: proof sandbox 3 ca trên macOS/Windows (F6D-22).** Proof bị chặn ở máy của user;
  `execute_code` fail-closed trên cả hai OS cho tới khi log proof 3 ca thật (sentinel deny +
  network deny + allowed read, cùng tiêu chuẩn với proof Linux) được ghi trên máy đó và trình
  review. Không có mốc thời gian cam kết. Điều kiện đóng cổng là log proof, không phải lời hứa.
- **Chạy thật trên Mac / Windows / Android / Wayland** (conformance thật, hotkey toàn cục
  macOS/Windows, portal Wayland): mọi khẳng định theo OS trong §5–§6 ngoài Linux/vega đều
  ghi "chưa kiểm chứng" và fail-closed.
- **zbus** (D-Bus cho Wayland/Linux): ngoài phạm vi; không quyết dùng hay không.
- **Ký số** (uiAccess Windows, ký macOS): ngoài phạm vi; không quyết đường ký.
- **Huấn luyện thật** (fine-tune): cổng phần cứng + training adapter thuộc Phase 5, không
  thay đổi trong Phase 6; không quyết gì thêm.
- **Engine voice / proactive / scheduler** (ASR/TTS, sweep, heartbeat, lịch): không thuộc
  Phase 6; không quyết.
- **Đồng bộ liên body Phase 7** (protocol v2, conflict, pairing đa body): id agent trong bản
  này chỉ có ý nghĩa trong 1 tiến trình; đồng bộ liên máy là thiết kế Phase 7.
- **Best-of-N Phase 8** (Agent S số 7, P1, có trần chi phí): không thuộc Phase 6; không quyết.
- **Grounder D12** (model grounding chuyên dụng, cloud/local theo Privacy Mode): không thuộc
  Phase 6; R4 dùng verifier quan sát hiện có, không phán xét grounding.
- Ngoài ra, bản này cũng KHÔNG quyết: transport MCP qua HTTP (§6.1 đã quyết KHÔNG làm),
  seccomp filter / Landlock (§5.1), risk theo tool MCP (§6.2), độ ưu tiên lập lịch (§2.1),
  hierarchy sâu hơn 1 tầng (§8.1), hash pinning MCP server (§6.3, F6D-21), cgroup giới hạn
  tài nguyên (§16, F6D-23), đặt NPROC bên trong namespace (§5.1), probe Apple Silicon. Các mục
  này là quyết định "không làm ở Phase 6", khác với "chưa quyết" — review cần phân biệt hai
  loại khi sign-off.

---

## 14. Vòng review + sign-off (cổng G6-10)

Vòng review thiết kế 4 AI (Claude + Dola kiến trúc, ChatGPT UX, Gemini red-team/hiệu năng)
theo đúng kỷ luật review các phase trước (CRITERIA P6.0). **Đã nhận 3/4 bài.** Phần kiến trúc
Q1–Q3 vẫn có phủ đầy đủ nhờ Dola (cùng phần với Claude theo quy tắc review từ Phase 5). Vòng
review được coi là đủ để tổng hợp; sự vắng mặt của Claude được ghi rõ dưới đây, không giả vờ
là 4/4. Toàn văn 3 bài: `design-review-chatgpt.md`, `design-review-gemini.md`,
`design-review-dola.md`; tổng hợp: `REVIEW-SYNTHESIS-DESIGN.md`.

### 14.1 Bảng sign-off

| Reviewer | Phần | Model | Đọc gì | Verdict | Ngày |
|---|---|---|---|---|---|
| ChatGPT | Q4–Q6 (UX) | GPT-6 | Bản tóm tắt | APPROVE-WITH-CHANGES (4 Major, 3 Minor) | 18/11/2026 |
| Gemini | Q7–Q8 (red-team/hiệu năng) | Argon theo bộ chọn model trên trang; model tự khai "Gemini" trong bài — ghi cả hai | Toàn bộ DESIGN-DRAFT.md | APPROVE-WITH-CHANGES (2 Blocker, 4 Major, 1 Minor) | 18/11/2026 |
| Dola | Q1–Q3 (kiến trúc) | Dola | Bản tóm tắt | APPROVE-WITH-CHANGES (0 Blocker, 4 Major, 2 Minor) | 18/11/2026 |
| Claude | Q1–Q3 (kiến trúc) | — | **KHÔNG CÓ BÀI** — task chết ở đăng nhập: Cloudflare → chuyển môi trường → mất phiên → chờ mã tới wmpsale18@gmail.com; Gmail chưa kết nối; theo lệnh KHÔNG HỎI LẠI của user nên dừng, không gửi gì. Phần Q1–Q3 vẫn phủ bởi Dola. | — | 18/11/2026 |

HEAD xuất phát: `b48dae9241659b0fea9aa80b5b8bfb258229a0ab`. Ngày chốt bản cuối: 18/11/2026.

### 14.2 Phán quyết từng điểm §14 của bản nháp (D-DESIGN-1..7)

1. **Quyết định G6-9 (§4): GIỮ HƯỚNG** (đồng thuận 3/3, D-DESIGN-1) — lỗi body → tất cả; lỗi
   khác → 1 agent; hết cap toàn cục → tất cả; kèm 3 điều kiện của ChatGPT (§4.1).
2. **Phân biệt MCP unsandboxed (§6.3): SIẾT THÀNH NGOẠI LỆ BẢO MẬT CÓ CHỦ ĐÍCH**
   (D-DESIGN-2) — mặc định false mọi OS; Linux từ chối `unsandboxed=true`; macOS/Windows
   opt-in từng server + xác nhận cấp server + cảnh báo + chỉ báo liên tục + audit + không tự
   fallback; Android không MCP; file cấu hình ngoài tầm ghi của tool/workdir.
3. **Phiên bản MCP (§6.1): GIỮ LEGACY CHO PHASE 6 + ADR BẮT BUỘC** (D-DESIGN-3) — chỉ thế hệ
   handshake `2025-06-18`/`2025-11-25`; từ chối `2026-07-28` kèm thông báo rõ; cấm giả lập
   handshake; mốc deprecation 7/2027; ADR `docs/adr/0001-mcp-legacy-protocol.md`; URL nguồn
   spec nhúng tại §6.1.
4. **Ledger token + reserve/settle (§3.3): GIỮ + SEMANTICS ĐẦY ĐỦ** (đồng thuận có điều kiện
   3/3, D-DESIGN-4) — 4 loại dữ liệu tách bạch; không giải phóng toàn bộ reserve khi chưa biết
   có tiêu thụ hay không; chống double-count theo `attempt_id`; UI ghi ƯỚC TÍNH; `max_tokens`
   mặc định 4096.
5. **Ngưỡng độ trễ hierarchy (§8.3): TÁCH CHỈ TIÊU** (D-DESIGN-5) — overhead thuần ≤10% (stub);
   wall e2e Ollama CPU 2 nhân ≤35–40%; token ≤2×; nối tiếp ≤10%; đạt ngưỡng không tự bật mặc
   định; vượt ngưỡng ở lại thử nghiệm + docs ghi số thật; không sửa config âm thầm; báo cáo
   đủ 4 số + median + ca thất bại.
6. **Guard R6 (§7.1): ĐỊNH NGHĨA FINGERPRINT + NGOẠI LỆ CUỘN BẰNG HASH FRAME** (đồng thuận
   3/3, D-DESIGN-6) — fingerprint `(tool_name, canonicalized_json_args)` + Δ≤2px; cuộn chỉ
   tính lặp khi hash frame không đổi; ngưỡng 5 cấu hình được; test dương tính giả bắt buộc;
   bác đường user-vượt-guard trong lượt.
7. **Ngoại lệ G6-8 (§0.2/§1.4): CHẤP NHẬN + TƯƠNG THÍCH SCHEMA** (đồng thuận 3/3,
   D-DESIGN-7) — trường `agent_id` first-class ngay cả khi cờ tắt; record cũ đọc được
   (mặc định `agent-001`); record mới bắt buộc có; `agent-000` cho hệ thống; viewer giấu cột
   khi 1 agent; test fixture JSONL cũ.

### 14.3 Điều kiện sign-off (đã đủ)

1. Đủ F6D-01..F6D-24 đã áp vào văn bản — đối chiếu tại
   `DESIGN-FINAL-CHECKLIST.md` (coordinator giai đoạn 2 đối chiếu từng mục).
2. Proof sandbox chạy LẠI với bộ cờ mới (F6D-01 + F6D-04) — 3 ca + baseline + kiểm chứng mới
   D/E/F, log mới `design-evidence/04-sandbox-proof-v2.log` (+ probe bổ sung
   `05-rlimit-scope.log`).
3. Bảng sign-off trên ghi đủ: 3 verdict APPROVE-WITH-CHANGES + Claude không có bài (lý do) +
   ngày + HEAD xuất phát.
4. CẤM commit code Phase 6 trước khi 1–3 xong. CẤM động README gốc + assets. Bản nháp
   (`research/andee/phase6/DESIGN-DRAFT.md`) giữ lại làm hồ sơ soạn thảo, không push lên repo.

---

## 15. UI đa agent (từ review ChatGPT — F6D-16..F6D-19)

Mục này là một phần của bản thiết kế đã sign-off (§14). Mọi trạng thái/thẻ/thông báo dưới đây
đều là yêu cầu đối với UI; CLI in các thông tin tương đương dưới dạng text.

### 15.1 Mô hình trạng thái đa agent thống nhất (F6D-16)

Mỗi agent tại mỗi thời điểm mang đúng một trạng thái hiển thị trong 9 trạng thái sau, cộng
trạng thái body toàn cục. Trạng thái hiển thị suy ra từ vòng đời §1.1 + xác minh §7.2 + phạm
vi lỗi §4 + ngân sách §3.3 + degraded §10 — không có trạng thái hiển thị nào không gắn với
một trạng thái nội bộ thật.

| # | Trạng thái hiển thị | Nghĩa (điều kiện vào) | Thoát khi |
|---|---|---|---|
| 1 | Đang chờ | Agent đã spawn, chưa bắt đầu lượt (đang xếp hàng spawn / chờ lượt) | Bắt đầu lượt → theo trạng thái lượt; bị hủy → Đang hủy |
| 2 | Đang chờ phê duyệt | Agent đang chặn ở một thẻ phê duyệt (có approval đang chờ) | Thẻ được trả lời → tiếp tục lượt; agent bị hủy → Đang hủy; thẻ hết hạn → lượt tiếp tục theo luật guard |
| 3 | Đang hủy | Đã gửi yêu cầu hủy, đang chờ kết thúc được kiểm chứng (§10.1 đường 3) | Kết thúc đã kiểm chứng → Đã hủy; quá bound 5s → Kết quả chưa xác định + Degraded (xem §15.3) |
| 4 | Đã hủy | Kết thúc do hủy, đã kiểm chứng (join xong, không task/con nào còn sống) | Trạng thái cuối |
| 5 | Kết quả chưa xác định | `UnknownOutcome` — action có thể đã thực thi một phần, không biết kết quả (lỗi body, hủy không kiểm chứng được) | Chỉ thoát bằng lượt mới có xác minh lại; không bao giờ tự chuyển thành Đã hủy/Hoàn thành |
| 6 | Kết thúc chưa xác minh | Lượt kết thúc nhưng `all_verified=false` (có bước chưa qua xác minh, §7.2) | Xác minh bổ sung thành công → Hoàn thành (ghi rõ xác minh muộn trong audit); nếu không, giữ nguyên |
| 7 | Degraded | Coordinator ở trạng thái degraded (có `task_end_unverified`, không spawn mới — §10.1) | Khởi động lại; không tự phục hồi âm thầm |
| 8 | Hoàn thành một phần | Cha tiếp tục sau khi con lỗi/timeout, kết quả cuối thiếu phần của con (§8.1, §15.3) | Trạng thái cuối của lượt đó (ghi rõ phần thiếu) |
| 9 | Chờ ngân sách | Agent dừng vì hết ngân sách theo agent (`budget_exhausted`), chờ user tăng ngân sách hoặc hủy lượt | Ngân sách được tăng → tiếp tục; user hủy → Đã hủy |

**Trạng thái "Body chưa sẵn sàng"** (toàn cục, không gắn một agent): body actor đang broken /
đang rebuild / chưa khởi động xong — không agent nào được gửi action mới. Hiển thị tách khỏi
trạng thái từng agent; khi body chưa sẵn sàng, mọi agent có action đang chờ đều hiển thị kèm
lý do này thay vì im lặng chờ.

**Phân biệt Cancelled với UnknownOutcome (bắt buộc):** "Đã hủy" chỉ dùng khi kết thúc đã kiểm
chứng; mọi trường hợp chưa kiểm chứng được đều là "Kết quả chưa xác định", kể cả khi lệnh hủy
đã gửi. Ba loại agent sau lỗi fail-all (§4.1c) hiển thị phân biệt: agent thất bại (gây lỗi),
agent bị dừng theo hệ thống (lan truyền), kết quả chưa xác định.

### 15.2 Thẻ phê duyệt đa agent (F6D-17)

Mỗi thẻ phê duyệt phải chứa đủ ngữ cảnh sau (khi cờ song song tắt, thẻ hiển thị y hệt hôm nay
trừ các trường vốn đã có; các trường đa agent chỉ hiện khi có từ 2 agent trở lên hoặc khi
thông tin đó tồn tại):

- Vị trí hàng đợi: `1/2`, `2/2`… (thứ tự thẻ này trong hàng đợi thẻ) + số yêu cầu đang chờ.
- Thời gian chờ đã trôi + thời điểm hết hạn (timeout thẻ 60s mặc định).
- Nhãn nhiệm vụ của agent + tên agent (khi cờ bật).
- Hành động cụ thể sẽ chạy (tool + mô tả hành động, không chỉ tên tool thô).
- Nhãn mức độ ảnh hưởng: **đọc / ghi / thực thi / gửi ra ngoài** (một nhãn chính + nhãn phụ
  nếu có; tool MCP luôn gắn ít nhất "thực thi" vì không biết server làm gì — §6.2).
- Tóm tắt args con người đọc được (không dump JSON thô; JSON đầy đủ xem được khi mở rộng).
- MCP server + tên tool, nếu là tool MCP; kèm chỉ báo unsandboxed liên tục nếu server đang
  chạy theo ngoại lệ §6.3.
- Ước tính token của lần gọi liên quan (nếu có) + cảnh báo khi không đo được
  (`usage_unmeasured`, §3.3) — không hiển thị USD khi chỉ có reserve (§15.4).
- Phạm vi phê duyệt: **"chỉ lần này"** — quyết định chỉ áp dụng cho đúng lần gọi này
  (`agent_id` + `request_id`), không có "luôn cho phép" trong Phase 6.
- Với `execute_code`: hash code rút gọn có thể mở rộng xem đủ + kiểm chứng được: UI hiển thị
  hash đã phê duyệt và, sau khi chạy, xác nhận hash thực thi == hash phê duyệt (khớp thì ghi
  rõ "đã kiểm chứng", lệch thì đó là lỗi không bao giờ được im lặng — xem §5.1, args immutable).

### 15.3 Điều khiển dừng (F6D-18)

- **Hai thao tác tên khác nhau, không gộp:**
  - **"Dừng agent này"** — dừng 1 agent + các con của nó (hủy lan truyền xuống, §8.1); các
    agent khác tiếp tục. Không trip stop latch toàn cục (trừ khi agent đó gây lỗi body).
  - **"DỪNG TẤT CẢ"** — dừng toàn hệ thống + trip stop latch; mọi agent dừng, mọi action đang
    xếp hàng bị deny, body stack phải rebuild mới chạy lại (§4).
- **"DỪNG TẤT CẢ" có cảnh báo phạm vi** trước khi thực thi (trừ trường hợp khẩn cấp có chủ
  đích — nút khẩn cấp chuyên biệt, không qua cảnh báo, nhưng vẫn ghi audit đầy đủ). Cảnh báo
  nêu rõ: mọi agent sẽ dừng, latch sẽ trip, và thread body đang kẹt (nếu có) không giật được
  (§10.1).
- **Trạng thái chuyển tiếp (bắt buộc):** đã gửi yêu cầu → đang chờ kết thúc → đã xác nhận /
  không xác minh được. UI hiển thị đúng giai đoạn đang diễn ra; **CẤM đổi sang "Đã dừng" chỉ
  vì đã gọi `abort()`** — "Đã hủy" chỉ hiện sau khi kết thúc đã kiểm chứng (§10.1 đường 3);
  quá bound thì hiện "Kết quả chưa xác định" + Degraded, không hiện "Đã dừng".
- **Kết quả một phần:** khi con lỗi mà cha tiếp tục, UI hiển thị kết quả một phần rõ ràng:
  phần nào đã xong, phần nào thiếu do con lỗi, và ảnh hưởng tới kết quả cuối (cha quyết định
  tiếp tục dựa trên phần còn lại). Không báo "hoàn thành" chung chung khi có phần thiếu.

### 15.4 Thông báo lỗi hướng user (F6D-19)

Năm tình huống dưới đây có thông báo theo cấu trúc 3 phần bắt buộc: **(1) chuyện gì đã xảy
ra, (2) ảnh hưởng gì, (3) làm gì tiếp**. Mã kỹ thuật (`sandbox_unavailable`,
`body_queue_full`, `budget_exhausted`, `queue_sla_expired`, `task_end_unverified`) giữ trong
audit + hiển thị phụ (mở rộng xem được), không phải nội dung chính của thông báo. **CẤM đề
nghị tắt sandbox** trong mọi thông báo. **CẤM hiển thị USD có vẻ chính xác khi chỉ có
reserve** — chỉ hiển thị USD khi có bảng giá + usage đã đo (§3.3); nếu không thì ghi "chưa đo
được chi phí" hoặc ước tính token kèm nhãn ƯỚC TÍNH. Mẫu cấu trúc (lời văn cố định trừ khi
review đổi):

1. **Sandbox không khả dụng** — (1) Không chạy được code vì sandbox chưa sẵn sàng trên máy
   này. (2) Lệnh `execute_code` đã bị từ chối; không có code nào chạy ngoài sandbox. (3) Kiểm
   tra cài đặt sandbox (Linux: bubblewrap) rồi thử lại; trên macOS/Windows, sandbox chưa được
   kiểm chứng — xem cổng mở §13. Không có đường tắt nào.
2. **Hàng đợi đầy** — (1) Hàng đợi action đã đầy. (2) Action mới đã bị từ chối ngay, không
   xếp hàng thêm; các action đang chờ không bị mất. (3) Chờ actor xử lý bớt rồi gửi lại, hoặc
   dừng bớt agent đang chạy.
3. **Hết ngân sách** (theo agent / toàn cục / không đo được) — (1) Ngân sách token đã hết
   [của agent này / của toàn hệ thống / không đo được mức tiêu thụ nên phải dừng thận trọng].
   (2) [Chỉ agent này dừng / Tất cả agent dừng / Lượt dừng để tránh vượt chi phí không kiểm
   soát]. Số hiển thị là ƯỚC TÍNH, không phải hóa đơn. (3) Tăng ngân sách trong cấu hình rồi
   chạy lượt mới, hoặc chấp nhận dừng tại đây.
4. **Hết hạn SLA** — (1) Action đã chờ quá lâu trong hàng đợi nên hết hạn. (2) Action không
   chạy (chạy muộn lên màn hình đã đổi sẽ sai); agent sẽ chụp lại màn hình mới. (3) Không cần
   làm gì — lượt sẽ tự tiếp tục với trạng thái mới; nếu lặp lại, giảm số agent hoặc tăng SLA
   trong cấu hình (thời gian chờ tối đa, §12).
5. **Kết thúc không xác minh được (`task_end_unverified`)** — (1) Không xác minh được agent
   đã dừng hẳn sau lệnh hủy. (2) Hệ thống chuyển sang Degraded: không spawn agent mới cho tới
   khi khởi động lại; kết quả của agent đó là "chưa xác định". (3) Khởi động lại ứng dụng để
   về trạng thái sạch; kiểm tra audit để biết action cuối cùng đã biết chắc tới đâu.

Bề mặt cấu hình phân 3 tầng (cơ bản/nâng cao/chuyên gia) và quy tắc trình bày từng setting
nằm ở §12 (F6D-19) — UI cấu hình và UI thông báo tuân cùng một kỷ luật trình bày này.

### 15.5 Badge xác minh (F6D-14)

Badge trạng thái xác minh hiển thị 5 cách phân biệt, gắn với §7.2: "Đã xác minh bằng giao
diện" (`Verified(gui)`), "Đã đọc lại artifact" (`Verified(artifact)` — không gọi là "xác minh
bằng giao diện"), "Chưa xác minh" (`Unverified`), "Xác minh thất bại" (`Failed`), "Xác minh
không khả dụng" (`Unavailable`). Badge "Chưa xác minh"/"Thất bại"/"Không khả dụng" không bao
giờ đi kèm nhãn "hoàn thành" chung chung.

### 15.6 Kiểm chứng (UI)

- `ui_agent_state_model`: mỗi trạng thái nội bộ map đúng một trạng thái hiển thị §15.1; 9
  trạng thái + "Body chưa sẵn sàng" đều có thể đạt được trong harness; Cancelled và
  UnknownOutcome không bao giờ hiển thị trùng nhau.
- `ui_approval_card_contents` (F6D-17): thẻ render đủ mọi trường §15.2 (vị trí hàng đợi, nhãn
  mức độ ảnh hưởng, ước tính token + cảnh báo khi không đo được, phạm vi "chỉ lần này", hash
  code + kiểm chứng khớp); cờ tắt → thẻ như hôm nay.
- `ui_stop_transitions` (F6D-18): hai thao tác dừng tên khác nhau; "DỪNG TẤT CẢ" có cảnh báo
  phạm vi; trạng thái chuyển tiếp đúng thứ tự; "Đã dừng" không xuất hiện chỉ vì `abort()` đã
  gọi (ép bằng harness: abort chưa join → UI vẫn ở "Đang hủy"); kết quả một phần hiển thị đủ
  phần thiếu + ảnh hưởng.
- `ui_error_messages` (F6D-19): 5 tình huống đều có đủ 3 phần; mã kỹ thuật trong audit +
  hiển thị phụ; grep cổng: không thông báo nào chứa đề nghị tắt sandbox; không USD nào hiển
  thị khi chỉ có reserve (test với ledger chỉ-reserve → UI ghi "chưa đo được"/ước tính).
- `ui_verification_badges` (F6D-14): 5 giá trị badge hiển thị phân biệt đúng §15.5.

---

## 16. Giới hạn tài nguyên giữa các agent (F6D-23)

- **Phase 6 KHÔNG ép CPU/RAM từng agent.** Không cgroup, không sudo, không giới hạn tài nguyên
  theo agent ở tầng OS. Một agent nặng (vòng lặp provider dày, tool chạy lâu, code sinh nhiều
  output) có thể làm chậm toàn hệ thống — đây là giới hạn đã biết, phải ghi trong tài liệu vận
  hành, không tuyên bố cô lập tài nguyên.
- **Giới hạn được ép là:** body serialization (một action tại một thời điểm, §2) + trần hàng
  đợi (16/agent, 64 toàn cục, §9) + ngân sách token (theo agent + toàn cục, §3.3) + giới hạn
  sandbox cho từng lần chạy code (tmpfs 64 MiB, RLIMIT_FSIZE/AS, timeout — §5.1; các giới hạn
  này áp dụng cho từng lần chạy `execute_code`, không phải cho từng agent). Ngoài các giới
  hạn đó, các agent chia sẻ tài nguyên máy một cách không phân biệt.
- **Hệ quả vận hành:** số agent mặc định 3 (§1.1) là lựa chọn cho máy yếu; tăng `max_agents`
  không đảm bảo tăng tốc và có thể làm chậm hệ thống (xem §8.3, §9.1, §12). Tài liệu vận hành
  phải nêu rõ: nếu hệ thống chậm khi nhiều agent chạy, giảm số agent / giảm tải từng agent là
  biện pháp duy nhất ở Phase 6 — không có knob cô lập CPU/RAM nào.
- **Kiểm chứng:** `resource_limits_documented` — test cổng grep: tài liệu vận hành chứa tuyên
  bố giới hạn này (không ép CPU/RAM từng agent, không cgroup) + liệt kê đúng các giới hạn
  được ép. Không có test nào được tuyên bố "cô lập tài nguyên" khi cơ chế không tồn tại.

---

## Phụ lục: danh mục bằng chứng

- `design-evidence/01-env-probe.log`, `02-userns-probe.log`, `03-sandbox-proof.log`,
  `04-sandbox-proof-v2.log`, `05-rlimit-scope.log` — log kiểm chứng sandbox trên vega (§0.3).
- `design-evidence/probe-01-env.sh`, `probe-02-userns.sh`, `probe-03-proof.sh`,
  `probe-03-inner.sh`, `probe-04-proof.sh`, `probe-04-inner.sh`, `probe-05-rlimit-scope.sh`,
  `probe-05-inner.sh` — script tái lập (bản trên vega: `/home/adminis/mimint-p6-probe-*.sh`
  và `/home/adminis/mimint-p6-inner.sh`, `/home/adminis/mimint-p6-inner-v2.sh`,
  `/home/adminis/mimint-p6-inner-05.sh`).
- Cây staging phục vụ viết bản cuối: `/home/adminis/mimint-phase6-design` tại
  `b48dae9241659b0fea9aa80b5b8bfb258229a0ab` (chưa commit, chưa push; coordinator đối chiếu
  rồi tự push). Cây chỉ-đọc phục vụ đọc code giai đoạn 1:
  `/home/adminis/mimint-p6-design-read` (cùng HEAD).
- Hồ sơ review: `DESIGN-DRAFT.md` (bản nháp), `design-review-chatgpt.md`,
  `design-review-gemini.md`, `design-review-dola.md` (toàn văn 3 bài),
  `REVIEW-SYNTHESIS-DESIGN.md` (7 quyết định + 24 mục sửa), `DESIGN-FINAL-CHECKLIST.md`
  (đối chiếu từng mục F6D/D-DESIGN với § + dòng trong bản này).
- ADR: `docs/adr/0001-mcp-legacy-protocol.md` trong repo (lý do giữ legacy handshake + điều
  kiện mở rộng + mốc deprecation 7/2027).
- Tra cứu protocol MCP: revision 2024-11-05 → 2026-07-28, thế hệ handshake tới 2025-11-25,
  thế hệ stateless từ 2026-07-28. URL nguồn nhúng tại §6.1 + ADR, đã kiểm chứng mở được ngày
  2026-11-18: blog MCP 28/07/2026 và spec 2026-07-28.

— Hết bản thiết kế cuối. —

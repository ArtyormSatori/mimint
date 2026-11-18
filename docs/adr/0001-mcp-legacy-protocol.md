# ADR 0001 — Giữ thế hệ handshake legacy của MCP cho Phase 6, chưa hỗ trợ 2026-07-28

- **Trạng thái:** Đã chấp nhận (18/11/2026, cùng sign-off thiết kế Phase 6 — xem
  `docs/phase6-design.md` §14, quyết định D-DESIGN-3).
- **Bối cảnh:** repo chưa có quy ước ADR nào trước file này (đã kiểm tra cây tại HEAD
  `b48dae9`: không có thư mục `docs/adr*`, không có file ADR nào). File này lập quy ước:
  ADR đặt tại `docs/adr/`, đánh số tăng dần `NNNN-ten-ngan.md`, viết bằng tiếng Việt như
  các tài liệu thiết kế khác của repo.

## Bối cảnh

Đặc tả MCP hiện hành là revision `2026-07-28`: lõi stateless, bỏ trao đổi handshake
`initialize`/`initialized` và header `Mcp-Session-Id`; mỗi request tự mang version +
capability trong `_meta`; client muốn biết capability trước thì gọi `server/discover`.
Các tính năng cũ (handshake, sampling, roots, logging) còn được hỗ trợ tối thiểu 12 tháng
(tới tháng 7/2027) theo chính sách deprecation của MCP. Nguồn (đã kiểm chứng mở được ngày
18/11/2026):

- Blog MCP 28/07/2026: <https://blog.modelcontextprotocol.io/posts/2026-07-28/>
- Spec 2026-07-28: <https://modelcontextprotocol.io/specification/2026-07-28>

Thiết kế Phase 6 (`docs/phase6-design.md` §6) khóa phạm vi MCP client ở: transport stdio duy
nhất, thế hệ handshake (`2025-06-18`/`2025-11-25`), subset `tools/list` + `tools/call`, không
batching, không capability phía client (không roots, không sampling).

## Quyết định

Phase 6 CHỈ triển khai thế hệ handshake legacy trên stdio:

1. Client đề xuất `2025-06-18` trong `initialize`; chấp nhận server trả về `2025-06-18`
   hoặc `2025-11-25`.
2. Server chỉ hỗ trợ stateless (`2026-07-28`) hoặc version lạ → từ chối, không nối, không
   đăng ký tool nào, kèm thông báo rõ "server yêu cầu phiên bản không được hỗ trợ" + ghi
   audit.
3. **Cấm giả lập handshake cũ** với server chỉ-stateless: không gửi `tools/list` với version
   giả định, không tự "dịch" stateless thành handshake.

## Lý do

- Phạm vi Phase 6 đã khóa trước khi spec mới được xem xét kỹ; triển khai lõi stateless đúng
  (per-request version/_meta, `server/discover`, MRTR cho sampling/elicitation, định tuyến
  bằng header) là một thiết kế riêng, không phải "thêm version string".
- Thế hệ handshake vẫn được hỗ trợ tới tháng 7/2027 theo chính sách deprecation — giữ legacy
  cho Phase 6 không đồng nghĩa dùng protocol đã chết, và mọi server handshake hiện có đều
  nối được.
- Subset handshake đã được kiểm chứng bằng server giả tự viết (§6.6 của thiết kế); một bản
  triển khai stateless nửa vời sẽ nguy hiểm hơn là từ chối rõ ràng.

## Điều kiện mở rộng sang 2026-07-28

Việc hỗ trợ `2026-07-28` chỉ bắt đầu khi đủ:

1. Thiết kế bổ sung + review riêng cho lõi stateless (per-request version/_meta,
   `server/discover`, MRTR, header `Mcp-Method`/`Mcp-Name` nếu làm HTTP).
2. Ma trận test bằng server giả cho `2026-07-28` (tương đương `mcp_version_negotiation_matrix`
   hiện có), gồm cả server vừa hỗ trợ handshake vừa hỗ trợ stateless.
3. ADR này được sửa đổi (trạng thái mới + ngày) theo cùng kỷ luật review như thiết kế.

Mốc deprecation tháng 7/2027 là động lực thời gian: trước mốc đó phải có quyết định mở rộng
hoặc quyết định ở lại legacy có ghi rõ hệ quả (server mới chỉ-stateless sẽ không nối được).

## Hệ quả

- Server chỉ hỗ trợ `2026-07-28` bị từ chối trong Phase 6, với thông báo rõ ràng (không im
  lặng, không lỗi chung chung). User dùng server như vậy phải chờ bản mở rộng hoặc dùng server
  hỗ trợ handshake.
- Không code nào trong Phase 6 được phép gửi request stateless hay giả lập handshake — ép bằng
  test `mcp_no_faked_handshake` + grep cổng (§6.6 của thiết kế).
- ADR này không quyết gì về transport HTTP (Streamable HTTP vẫn KHÔNG triển khai ở Phase 6
  theo §6.1 của thiết kế); mở rộng stateless và mở rộng HTTP là hai quyết định riêng.

## Tham chiếu

- `docs/phase6-design.md` §6 (hợp đồng MCP client), §14 (D-DESIGN-3).
- `REVIEW-SYNTHESIS-DESIGN.md` (tổng hợp review, ngoài repo).

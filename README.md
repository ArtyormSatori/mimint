# Mimint

> A personal AI assistant, built from scratch in Rust — for macOS, Android, Windows and Linux.
>
> Trợ lý AI cá nhân, viết từ đầu bằng Rust — cho macOS, Android, Windows và Linux.

<p align="center">
  <img src="assets/mint-thinking.jpg" width="360" alt="Mint sitting, thinking, waiting">
</p>

Đây là Mint — một cục bông bạc hà nhỏ, khuôn mặt của Mint chính là mặt của trợ lý. Trong ảnh, Mint đang ngồi suy nghĩ, chờ bạn. Biểu cảm của Mint đi theo việc trợ lý đang làm, điều khiển bằng trạng thái rõ ràng, không đoán từ văn bản.

This is Mint — a small mint-fluff companion whose face is the assistant's avatar. Here Mint sits thinking, waiting for you. Mint's expressions follow what the assistant is doing, driven by explicit state, never guessed from text.

**Trạng thái: đang nghiên cứu & thiết kế.** Repo này hiện chỉ có phần giới thiệu này và hình trên. Chưa có code nào được đăng. Code đầu tiên sẽ đến cùng Phase 0 (spike), và mọi điều dưới đây mô tả thiết kế *dự định*, không phải hành vi đã có.

**Status: research & design phase.** This repository currently contains this introduction and the image above. No code has been published yet. The first code will land with Phase 0 (spikes), and every claim below describes the *intended* design, not shipped behavior.

Mimint là trợ lý AI cá nhân: trò chuyện, giúp việc hằng ngày, và — khi bạn phê duyệt rõ ràng — điều khiển máy tính và trình duyệt của bạn. Mimint được thiết kế để nhanh, riêng tư, và thuộc về bạn: dữ liệu của bạn vẫn là của bạn, backend do bạn chọn, và không điều gì có hệ quả xảy ra nếu bạn chưa đồng ý.

Mimint is a personal AI assistant that chats, helps with everyday tasks, and — with your explicit approval — operates your computer and browser. It is designed to be fast, private, and yours: your data stays yours, your backends are your choice, and nothing consequential happens without you saying so.

## Nguyên tắc / Principles

- **Mã hóa, và vẫn nhanh / Encrypted, and still fast.** Mọi kết nối đều được mã hóa và xác thực theo thiết kế, ngân sách tốc độ đo trên phần cứng khiêm tốn. Bảo mật không bao giờ được là lý do trợ lý chạy chậm. / Every connection is encrypted and authenticated by design, with performance budgets measured on modest hardware. Security must never be the reason the assistant feels slow.
- **Privacy Mode mặc định BẬT / Privacy Mode on by default.** Khi Privacy Mode bật, xử lý nhạy cảm ở lại trên máy: không ảnh chụp màn hình hay nội dung màn hình thô nào rời khỏi máy. / When Privacy Mode is on, sensitive processing stays on the device: no screenshots or raw screen content leave the machine.
- **Backend do bạn chọn / Your backend, your choice.** Endpoint tương thích OpenAI trước (phủ các runtime local như Ollama và LM Studio), rồi tới Claude. Không bắt buộc một nhà cung cấp duy nhất. / OpenAI-compatible endpoints first (covering local runtimes such as Ollama and LM Studio), Claude support next. No single vendor is required.
- **Không điều gì có hệ quả mà không có phê duyệt / Nothing consequential without approval.** Hành động liên quan tới tiền, gửi tin nhắn, xóa dữ liệu hay đổi mật khẩu luôn hỏi trước — một phê duyệt cho một hành động. Đèn báo và nút dừng ngay luôn hiện khi trợ lý điều khiển màn hình. Mọi click, phím bấm và trang đã xem đều ghi vào audit log. / Actions involving money, sending messages, deleting data, or changing credentials always ask first — one approval per action. A visible indicator and an instant stop button are present whenever the assistant controls the screen. Every click, keypress, and page visit is written to an audit log.
- **Một não, nhiều thân / One brain, many bodies.** Một hồ sơ đặt tên được (persona, backend, giọng, kỹ năng, chủ động, riêng tư) có thể gán cho từng thiết bị. Các thân dự kiến gồm desktop, mobile và thiết bị đồng hành ESP32. / A named profile (persona, backend, voice, skills, proactivity, privacy) can be assigned to each device. Planned bodies include desktop, mobile, and an ESP32-based companion.
- **Trung thực về giới hạn / Honest about limits.** Phần cứng không chạy nổi tính năng nào thì tính năng đó hiện rõ là không khả dụng — không bao giờ âm thầm giảm chất lượng. / If the hardware cannot run a feature, the feature shows as unavailable — never silently degraded.

## Nền tảng / Platforms

macOS trước, rồi Android, Windows và Linux. Một lõi Rust chung; mỗi nền tảng một thân native.

macOS first, then Android, Windows, and Linux. One shared Rust core; a native body per platform.

## Lộ trình / Roadmap

1. **Phase 0 — Spike:** mật mã và truyền tải, grounding trên máy, chọn framework UI, baseline phần cứng. / Cryptography and transport, on-device grounding, UI framework choice, hardware baselines.
2. **Phase 1 — Core:** protocol v2 (identity, trao đổi khóa, mã hóa xác thực, pairing), hub, và hợp đồng body macOS. / Protocol v2 (identity, key exchange, authenticated encryption, pairing), hub, and the macOS body contract.
3. **Phase 2 — Trí nhớ, persona và kỹ năng / Memory, persona, and skills.**
4. **Phase 3 — Định tuyến, backend và chế độ riêng tư / Routing, backends, and privacy modes.**
5. **Phase 4 — Avatar, giọng nói và giao diện đầy đủ / Avatar, voice, and the full interface.**
6. **Phase 5 — Computer use và browser use, sau cổng phê duyệt / Computer use and browser use, behind the approval gates.**
7. **Phase 6 — Thân đồng hành, gồm ESP32 / Companion bodies, including ESP32.**
8. **Phase 7 — Củng cố, đánh giá và phát hành / Hardening, evaluation, and release.**

## Đóng góp / Contributing

Chưa nhận đóng góp. Thiết kế vẫn đang ổn định; quay lại khi Phase 0 lên sóng.

Not accepting contributions yet. The design is still settling; come back when Phase 0 lands.

## Giấy phép / License

Sẽ công bố cùng bản code đầu tiên. Hiện repo này chưa chứa code của bên thứ ba nào.

To be announced with the first code release. No third-party code is included in this repository today.

---

*Mimint là dự án độc lập, thiết kế và viết từ đầu. / Mimint is an independent project, designed and written from scratch.*

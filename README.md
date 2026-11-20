# Mimint

<p align="center">
  <img src="assets/mint-thinking.gif" width="360" alt="Mint thinking">
</p>

**Mimint** là trợ lý AI cá nhân viết từ đầu bằng Rust, cho macOS, Android, Windows và Linux — nhanh, riêng tư, thuộc về bạn. Đây là Mint.

**Mimint** is a personal AI assistant built from scratch in Rust, for macOS, Android, Windows and Linux — fast, private, yours. This is Mint.

*Phase 1–7 xong (839 test xanh trên main); Phase 8 đang làm: cứng hóa + đóng gói. / Phases 1–7 done (839 tests green on main); Phase 8 in progress: hardening + packaging.*

## Best-of-N (P8.3, eval harness)

Best-of-N chạy mỗi golden task N lần (temperature 0.7, seed base+i) rồi chọn bằng oracle → đa số (cụm 16px) → đầu tiên. **"Best-of-N (temperature=0.7) must be compared against single-shot (temperature=0.7); do not compare with temperature=0 baselines."** Điểm `pass_at_n_oracle` là trần trên nhờ oracle (oracle upper bound), không phải hiệu năng triển khai được — hiệu năng thật là `blind_pick_pass` (chọn không có oracle). Tập đo nhỏ (n=3) không kết luận thống kê. Không dùng model judge.

## License

Code theo Apache License 2.0 — xem [LICENSE](LICENSE). Nhân vật Mint (ảnh/video, mặt avatar 9 trạng thái, tên "Mint") KHÔNG thuộc Apache 2.0: cấm chỉnh sửa, tái sử dụng, phân phối lại và tạo phái sinh khi chưa có văn bản cho phép — xem [NOTICE](NOTICE) và [docs/character-license.md](docs/character-license.md). Thư viện bên thứ ba: xem [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).

## Disclaimer

Sản phẩm đang phát triển, không bảo hành ("AS IS", xem LICENSE §7). Privacy Mode mặc định bật: chặn kết nối ra ngoài theo thiết kế (đã kiểm chứng bằng strace ở Phase 2). Mimint là hệ độc lập, không pair với andee gốc.

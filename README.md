# Mimint

<p align="center">
  <img src="assets/mint-thinking.gif" width="360" alt="Mint thinking">
</p>

**Mimint** là trợ lý cá nhân viết từ đầu bằng Rust, chạy trên máy của bạn cho macOS, Android, Windows và Linux. Nó trò chuyện, giúp việc trên máy tính và đồng bộ mã hóa giữa các thiết bị của bạn — mặc định không có dữ liệu nào rời khỏi máy. Đây là Mint.

**Mimint** is a personal assistant built from scratch in Rust, running on your own machines for macOS, Android, Windows and Linux. It chats, helps on your computer, and syncs encrypted between your devices — by default, nothing leaves your machine. This is Mint.

*Phase 8 đóng 21/11 (913 test xanh trên main); bản phát hành đầu tiên v0.1.0. / Phase 8 done 21/11 (913 tests green on main); first release v0.1.0.*

## Tính năng

- Trò chuyện và giúp việc trên máy tính: mọi hành động có hệ quả (tiền, gửi, xóa) đều cần bạn phê duyệt từng lần, có nút dừng ngay.
- Đồng bộ mã hóa đầu cuối giữa các thiết bị của bạn, ghép nối bằng mã 6 số.
- Chế độ riêng tư mặc định bật: chặn kết nối ra ngoài, đã kiểm chứng bằng strace.
- Làm việc với dịch vụ ngôn ngữ bạn chọn: Ollama trên máy, Claude, OpenAI-compatible, LM Studio.
- Tự học tầng 1 từ quỹ đạo đã kiểm chứng; dữ liệu ở trên máy bạn.
- Mặt Mint 9 trạng thái, lồng tiếng song ngữ Việt–Anh.

## Features

- Chat and computer help: every consequential action (money, sending, deleting) needs your approval each time, with a stop-now button.
- End-to-end encrypted sync between your devices, paired with a 6-digit code.
- Privacy Mode on by default: blocks outbound connections, verified with strace.
- Works with the language service you choose: on-device Ollama, Claude, OpenAI-compatible, LM Studio.
- Tier-1 self-learning from verified trajectories; your data stays on your machines.
- Mint's face in 9 states, bilingual Vietnamese–English voice.

## Tải về / Download

Tải ở mục [Releases](https://github.com/ArtyormSatori/mimint/releases). Hiện có Linux x86_64; macOS, Windows và Android đang chờ chạy thật trên máy.

Get it from [Releases](https://github.com/ArtyormSatori/mimint/releases). Linux x86_64 is available now; macOS, Windows and Android builds are waiting on real-machine runs.

## Build

Cần Rust stable. / Requires stable Rust.

```sh
cargo build --release        # binaries: mimint-cli, mimint-app (target/release/)
./scripts/package-linux.sh   # Linux x86_64: build + đóng gói dist/mimint-linux-x86_64.tar.gz
```

## Best-of-N (P8.3, eval harness)

Best-of-N chạy mỗi golden task N lần (temperature 0.7, seed base+i) rồi chọn bằng oracle → đa số (cụm 16px) → đầu tiên. **"Best-of-N (temperature=0.7) must be compared against single-shot (temperature=0.7); do not compare with temperature=0 baselines."** Điểm `pass_at_n_oracle` là trần trên nhờ oracle (oracle upper bound), không phải hiệu năng triển khai được — hiệu năng thật là `blind_pick_pass` (chọn không có oracle). Tập đo nhỏ (n=3) không kết luận thống kê. Không dùng model judge.

## License

Code theo Apache License 2.0 — xem [LICENSE](LICENSE). Nhân vật Mint (ảnh/video, mặt avatar 9 trạng thái, tên "Mint") KHÔNG thuộc Apache 2.0: cấm chỉnh sửa, tái sử dụng, phân phối lại và tạo phái sinh khi chưa có văn bản cho phép — xem [NOTICE](NOTICE) và [docs/character-license.md](docs/character-license.md). Thư viện bên thứ ba: xem [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).

## Disclaimer

Sản phẩm đang phát triển, không bảo hành ("AS IS", xem LICENSE §7). Privacy Mode mặc định bật: chặn kết nối ra ngoài theo thiết kế (đã kiểm chứng bằng strace ở Phase 2). Mimint là hệ độc lập, không pair với andee gốc.

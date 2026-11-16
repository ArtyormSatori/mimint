# Mimint

> A personal AI assistant, built from scratch in Rust — for macOS, Android, Windows and Linux.

<p align="center">
  <video src="assets/mimint-thinking.mp4" width="360" autoplay muted loop playsinline controls></video>
</p>

<p align="center">
  <a href="assets/mimint-thinking.mp4">Watch the clip: Mint thinking, waiting for you</a>
</p>

This is Mint — a small mint-fluff companion whose face is the assistant's avatar. Here Mint sits thinking, waiting for you. Mint's expressions follow what the assistant is doing, driven by explicit state, never guessed from text.

**Status: research & design phase.** This repository currently contains this introduction and the clip above. No code has been published yet. The first code will land with Phase 0 (spikes), and every claim below describes the *intended* design, not shipped behavior.

Mimint is a personal AI assistant that chats, helps with everyday tasks, and — with your explicit approval — operates your computer and browser. It is designed to be fast, private, and yours: your data stays yours, your backends are your choice, and nothing consequential happens without you saying so.

*Tiếng Việt: Mimint là trợ lý AI cá nhân viết bằng Rust, chạy trên macOS, Android, Windows và Linux. Tiếng Việt là ngôn ngữ chính của sản phẩm.*

## Principles

- **Encrypted, and still fast.** Every connection is encrypted and authenticated by design, with performance budgets measured on modest hardware. Security must never be the reason the assistant feels slow.
- **Privacy Mode on by default.** When Privacy Mode is on, sensitive processing stays on the device: no screenshots or raw screen content leave the machine.
- **Your backend, your choice.** OpenAI-compatible endpoints first (covering local runtimes such as Ollama and LM Studio), Claude support next. No single vendor is required.
- **Nothing consequential without approval.** Actions involving money, sending messages, deleting data, or changing credentials always ask first — one approval per action. A visible indicator and an instant stop button are present whenever the assistant controls the screen. Every click, keypress, and page visit is written to an audit log.
- **One brain, many bodies.** A named profile (persona, backend, voice, skills, proactivity, privacy) can be assigned to each device. Planned bodies include desktop, mobile, and an ESP32-based companion.
- **Honest about limits.** If the hardware cannot run a feature, the feature shows as unavailable — never silently degraded.

## Platforms

macOS first, then Android, Windows, and Linux. One shared Rust core; a native body per platform.

## Roadmap

1. **Phase 0 — Spikes:** cryptography and transport, on-device grounding, UI framework choice, hardware baselines.
2. **Phase 1 — Core:** protocol v2 (identity, key exchange, authenticated encryption, pairing), hub, and the macOS body contract.
3. **Phase 2 — Memory, persona, and skills.**
4. **Phase 3 — Routing, backends, and privacy modes.**
5. **Phase 4 — Avatar, voice, and the full interface.**
6. **Phase 5 — Computer use and browser use, behind the approval gates.**
7. **Phase 6 — Companion bodies, including ESP32.**
8. **Phase 7 — Hardening, evaluation, and release.**

## Contributing

Not accepting contributions yet. The design is still settling; come back when Phase 0 lands.

## License

To be announced with the first code release. No third-party code is included in this repository today.

---

*Mimint is an independent project, designed and written from scratch.*

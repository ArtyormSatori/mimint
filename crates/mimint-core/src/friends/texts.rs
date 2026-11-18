//! Exact user-facing strings for Phase 7 (single source).
//!
//! The CLI and the GUI use these same constants, so the wording a
//! user sees never drifts between surfaces (§12.4). Every string
//! here is quoted from `docs/phase7-design.md`; do not paraphrase
//! them in callers.

/// Warning shown at the device-management screen and when the
/// data dir is deleted (§3.2, exact wording).
pub const KEY_LOSS_WARNING: &str = "Nếu mất file khóa này, thiết bị này sẽ mất danh tính bảo mật và phải ghép đôi lại từ đầu với từng thiết bị đã pair. Khóa không được sao lưu, không được đồng bộ, và không thể khôi phục. Đừng xóa thư mục dữ liệu nếu bạn chưa hiểu hậu quả này.";

/// Privacy Mode text (§9.1, exact wording). Shown as text with a
/// link to the paired-device list; never a shield icon alone.
pub const PRIVACY_MODE_TEXT: &str = "Privacy Mode đang bật. Provider egress bị giới hạn: dữ liệu chỉ tới backend trên máy này. Các kết nối sau VẪN được phép và vẫn truyền dữ liệu: đồng bộ dữ liệu với thiết bị đã ghép đôi; phê duyệt yêu cầu từ peer đã được cấp quyền; bàn giao nhiệm vụ giữa các thiết bị đã ghép đôi. Lưu ý: Privacy Mode không ngăn truyền dữ liệu qua kết nối trực tiếp tới thiết bị đã ghép đôi — dữ liệu sync, phê duyệt và handoff vẫn chạy qua LAN/tailnet tới peer đã pair. Hãy kiểm tra danh sách thiết bị đáng tin cậy. [Xem danh sách thiết bị đã ghép đôi]";

/// The five hub exposure states (§5.1, exact wording).
pub const HUB_EXPOSURE_LOOPBACK: &str = "Loopback — chỉ thiết bị hiện tại truy cập";
pub const HUB_EXPOSURE_LAN_ON: &str = "LAN đang bật — có thể nhận kết nối từ mạng nội bộ";
pub const HUB_EXPOSURE_PAIRING_MODE: &str = "Pairing mode — đang cho phép yêu cầu ghép đôi";
pub const HUB_EXPOSURE_DRIVER_CONNECTED: &str =
    "Driver đang kết nối — thiết bị khác đang có phiên điều khiển";
pub const HUB_EXPOSURE_LAN_UNPAIRED: &str =
    "LAN chưa pair — không chấp nhận điều khiển/sync từ peer lạ";

/// Note shown with the "close LAN now" control (§5.1).
pub const CLOSE_LAN_KEEPS_PAIRING: &str = "Đóng LAN không thu hồi pairing — các thiết bị đã pair vẫn còn trong danh sách và nối lại được khi bạn bật LAN.";

/// The seven handoff states (§7.2, exact wording).
pub const HANDOFF_STATE_NOT_HANDED_OFF: &str = "Chưa bàn giao";
pub const HANDOFF_STATE_AWAITING_ACCEPT: &str = "Đang chờ chấp nhận";
pub const HANDOFF_STATE_HANDED_OFF: &str = "Đã bàn giao";
pub const HANDOFF_STATE_HOLDING_CONTROL: &str = "Đang giữ quyền điều khiển";
pub const HANDOFF_STATE_REJECTED: &str = "Bị từ chối";
pub const HANDOFF_STATE_EXPIRED: &str = "Hết hạn";
pub const HANDOFF_STATE_UNKNOWN: &str = "Không xác định được trạng thái";

/// The four ESP32 status labels (§8.5, exact wording).
pub const ESP32_LABEL_LAB: &str = "Lab mode — không mã hóa";
pub const ESP32_LABEL_XIAOZHI_LAN: &str = "XiaoZhi LAN — xác thực giới hạn";
pub const ESP32_LABEL_V2: &str = "Firmware v2 — khóa đã pair";
pub const ESP32_LABEL_VOICE_UNSUPPORTED: &str = "Voice hoàn chỉnh — chưa hỗ trợ Phase 7";

/// Conflict-resolution consequence explanations (§4.4).
pub const CONFLICT_KEEP_MINE: &str = "Giữ bản của tôi — bản của peer sẽ bị thay thế bằng bản này ở lần đồng bộ sau; bản của peer vẫn xem lại được trong lịch sử xung đột.";
pub const CONFLICT_TAKE_PEER: &str = "Lấy bản của peer — bản trên thiết bị này sẽ bị thay thế bằng bản của peer ở lần đồng bộ sau; bản hiện tại vẫn xem lại được trong lịch sử xung đột.";
pub const CONFLICT_KEEP_BOTH: &str = "Giữ cả hai — bản của peer được lưu thành record mới riêng (xem quy tắc tên dưới đây); bản trên thiết bị này giữ nguyên.";
pub const CONFLICT_EDIT_MYSELF: &str = "Tôi tự sửa — mở trình chỉnh sửa với cả hai bản; bản bạn lưu sẽ thành version mới và thay thế cả hai bản ở lần đồng bộ sau.";
/// Shown next to the DISABLED "keep both" choice for SOUL/profile (§4.4).
pub const CONFLICT_KEEP_BOTH_DISABLED: &str = "Không thể giữ cả hai vì tên file/hồ sơ là khóa duy nhất — chỉ một bản được dùng. Hãy chọn một bản, hoặc tự sửa thành bản mới.";

/// Fixed selective-sync sentence about notebook scope (§4.7).
pub const SELECTIVE_SYNC_NOTEBOOK: &str = "Đồng bộ Notebook chỉ bao gồm ghi chú được đánh dấu Chia sẻ. Ghi chú riêng tư không rời thiết bị.";

/// Tombstone texts (§4.5).
pub const TOMBSTONE_STILL_PRESENT: &str =
    "Dữ liệu trên thiết bị này vẫn còn nguyên cho tới khi bạn xác nhận.";
pub const TOMBSTONE_90_DAY_LIMIT: &str = "Yêu cầu xóa này được giữ 90 ngày. Nếu một thiết bị không đồng bộ trong hơn 90 ngày, record đã xóa có thể xuất hiện lại như record mới khi thiết bị đó đồng bộ lại — dữ liệu không bị mất âm thầm, nhưng bạn sẽ cần xóa lại hoặc giải quyết như xung đột.";

/// Pairing notices (§3.4).
pub const PAIRING_EXPIRED: &str =
    "Pairing đã hết hạn sau 5 phút. SAS cũ không còn giá trị. Hãy bắt đầu lại từ đầu.";
pub const PAIRING_MISMATCH: &str =
    "Hai mã SAS không khớp. Ceremony đã dừng. Hãy bắt đầu lại từ đầu và kiểm tra fingerprint.";
pub const PAIRING_NOT_IN_MODE: &str =
    "Thiết bị kia chưa bật chế độ pairing. Hãy bật 'Cho phép pair' trên thiết bị đó rồi thử lại.";
pub const PAIRING_RESTART_GUIDE: &str =
    "Bấm 'Cho phép pair' lại trên thiết bị kia, rồi bắt đầu lại từ đầu.";

/// Wrong-code notice with remaining attempts (§3.4).
pub fn pairing_wrong_code(remaining: u32) -> String {
    format!(
        "Mã SAS không đúng. Kiểm tra lại 6 số trên màn hình thiết bị kia và thử lại. Còn {remaining} lần thử trước khi bị khóa tạm thời."
    )
}

/// Lockout notice with the retry time (§3.4). `retry_time` is a
/// pre-formatted HH:MM string in local time.
pub fn pairing_lockout(retry_time: &str) -> String {
    format!("Pairing từ thiết bị này đang bị khóa tới {retry_time} do 5 lần sai liên tiếp.")
}

/// Revoke / delete explanations (§3.6).
pub const REVOKE_EXPLANATION: &str =
    "Thu hồi: thiết bị vẫn còn trong danh sách nhưng không được phép kết nối tới thiết bị này nữa.";
pub const DELETE_EXPLANATION: &str = "Xóa: xóa thông tin thiết bị khỏi danh sách; nếu thiết bị đó kết nối lại sẽ được xem là thiết bị lạ.";
pub const REVOKE_LOCAL_ONLY: &str = "Thu hồi chỉ có hiệu lực trên máy này";

/// Rotate-identity confirmation warning (§3.6).
pub const ROTATE_WARNING: &str = "Danh tính bảo mật của thiết bị này sẽ thay đổi. Tất cả peer đã pair sẽ mất hiệu lực. Bạn phải ghép đôi lại từng thiết bị. Không có giai đoạn tương thích song song — key cũ bị từ chối ngay.";

/// Fingerprint skip warning (§3.3): shown before the user may
/// skip the fingerprint check; skipping is audited.
pub const FINGERPRINT_SKIP_WARNING: &str = "Bỏ qua kiểm tra fingerprint làm giảm khả năng phát hiện thiết bị giả mạo. Mã SAS 6 số chỉ chống kẻ tấn công đoán mò; fingerprint mới là lớp xác minh mạnh. Chỉ bỏ qua khi bạn chắc chắn đang pair đúng thiết bị. Việc bỏ qua sẽ được ghi vào nhật ký kiểm tra.";

/// Free-field guidance for todos/plans (§4.6). Guidance for the
/// user, NOT a secret scanner: no code scans these fields.
pub const TODO_FREE_FIELD_GUIDANCE: &str = "Đừng ghi mật khẩu, khóa API, hay bí mật nào vào ghi chú (notes) của todo hay nội dung (text) của plan — các trường này được đồng bộ sang thiết bị đã pair.";

/// XiaoZhi OTA trust limitation (§8.2, honest statement).
pub const OTA_TRUST_LIMIT: &str =
    "OTA XiaoZhi tin UUID trên LAN; đường này an toàn ở mức LAN tin cậy + tailnet, không phải E2E";

/// XiaoZhi board limitation (§8.1).
pub const XIAOZHI_BOARD_LIMIT: &str = "Chỉ dùng board Wi-Fi/ESP cho đường XiaoZhi nếu cần xác thực; board modem không được tuyên bố xác thực";

/// Voice limitation (§8.5).
pub const VOICE_LIMIT: &str = "Voice đầy đủ cần ASR/TTS, engine voice bị cấm ở Phase 7 nên audio trong v1 chỉ được ghi nhận + trả về emotion/ack, chưa thành turn hoàn chỉnh bằng giọng nói.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts_are_non_empty_and_exact() {
        assert!(KEY_LOSS_WARNING.contains("không thể khôi phục"));
        assert!(PRIVACY_MODE_TEXT.contains("VẪN được phép"));
        assert!(PRIVACY_MODE_TEXT.contains("Xem danh sách thiết bị đã ghép đôi"));
        assert_eq!(HANDOFF_STATE_UNKNOWN, "Không xác định được trạng thái");
        assert!(CONFLICT_KEEP_BOTH_DISABLED.contains("khóa duy nhất"));
        assert!(pairing_wrong_code(3).contains("Còn 3 lần thử"));
        assert!(pairing_lockout("14:30").contains("14:30"));
    }
}

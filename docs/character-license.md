# Điều kiện riêng cho nhân vật Mint (Mimint Character Licence)

> Bản tiếng Việt là bản gốc. Bản tiếng Anh bên dưới là bản dịch để
> tham khảo; khi có mâu thuẫn, bản tiếng Việt được ưu tiên.
> The Vietnamese text is authoritative. The English text below is a
> translation for reference; in case of conflict, the Vietnamese
> text prevails.

## 1. Phạm vi

"Nhân vật Mint" là nhân vật trợ lý AI của dự án Mimint: hình dáng,
màu sắc, khuôn mặt, tên gọi "Mint" gắn với nhân vật, và mọi hình
ảnh / video / animation thể hiện nhân vật đó.

Tài sản nhân vật trong repo này (đã kiểm chứng tại commit `dd883e0`):

| Tài sản | Vị trí trong repo |
|---|---|
| Ảnh/video Mint đang suy nghĩ | `assets/mint-thinking.gif`, `assets/mint-thinking.jpg`, `assets/mimint-thinking.mp4` |
| Mặt avatar — 9 trạng thái: Neutral, Happy, Listening, Thinking, Speaking, Surprised, Confused, Sleepy, Celebrating | Tệp dữ liệu thiết kế: `assets/mint-face.json` — tham số hình mắt/miệng, màu sắc (má hồng), độ nghiêng, nảy, chùm sao cho 9 trạng thái. Mã render chung đọc tham số từ tệp này (`crates/mimint-ui/src/face.rs` + `face_design.rs`, Apache 2.0); 9 mã cảm xúc (`EmotionCode` trong `crates/mimint-core/src/emotion.rs`) chỉ là tên gọi chung, không phải thiết kế. Không có file ảnh riêng cho từng trạng thái. Bộ tham số mặc định trung tính (mặt chung, không phải mặt Mint) nằm trong code (`FaceDesign::neutral`, Apache 2.0) để fork thay thế ngay khi xóa tệp thiết kế. |
| Tên nhân vật "Mint" | Tên gọi gắn với nhân vật trong toàn repo |

KHÔNG có trong repo này (cùng điều kiện khi được công bố, đừng nhầm
là nội dung repo): 20 look (`assets/looks/` không tồn tại trong
repo) và tài liệu thiết kế (`design/`). Đó là tài liệu local, chưa
đưa vào repo tại commit này. File lồng tiếng TTS của nhân vật KHÔNG
thuộc điều kiện này: giọng hiện tại là giọng máy TTS, không phải
giọng riêng. "Giọng riêng" ở đây nghĩa là giọng của một người thật,
hoặc giọng được tạo riêng cho dự án (thuê diễn viên lồng tiếng /
fine-tune trên giọng người thật) — khi nào có giọng riêng thì xét
lại. File giọng TTS hiện tại không có trong repo này và không phân
phối kèm sản phẩm.

Mã render chung vẽ mặt avatar (đọc tham số từ tệp thiết kế, vẽ
hình), docs kỹ thuật, test và script build KHÔNG thuộc phạm vi này
— chúng theo Apache License 2.0 (xem `LICENSE`). Ranh giới: code
thì Apache 2.0; tham số thiết kế khuôn mặt trong
`assets/mint-face.json` (hình mắt/miệng, màu sắc, 9 trạng thái mặt
lõi) thì thuộc điều kiện riêng này. Sửa logic/bug trong code render
thì được; sửa tham số hình dáng trong tệp thiết kế thì không (khi
chưa có văn bản cho phép).

Mã SPDX: code là `Apache-2.0` (mã SPDX chuẩn); phần điều kiện riêng
cho nhân vật không có mã SPDX — đây là điều khoản bổ sung ngoài
SPDX (ghi là `LicenseRef-Mimint-Character-1.0`).

## 2. Điều cấm (khi chưa có văn bản cho phép)

Khi chưa có văn bản cho phép của chủ repo, với tài sản nhân vật
Mint, bạn KHÔNG ĐƯỢC:

1. **Chỉnh sửa** — sửa hình dáng, màu sắc, khuôn mặt, tên gọi, hoặc
   bất kỳ chi tiết nào của nhân vật (kể cả sửa rồi dùng riêng, không
   phát hành).
2. **Tái sử dụng** — dùng lại hình ảnh / video / mặt avatar / tên
   "Mint" trong bất kỳ sản phẩm, ứng dụng, website, bài viết, video
   hay tài liệu nào khác, kể cả **nhúng vào sản phẩm khác**.
3. **Phân phối lại** — đăng lại, chia sẻ lại, hoặc đưa tài sản nhân
   vật vào bất kỳ bản phân phối nào (kể cả fork giữ nguyên tài sản).
4. **Tạo phái sinh** — tạo nhân vật mới, look mới, biến thể, fan-art
   thương mại, hoặc bất kỳ tác phẩm nào dựa trên Mint.
5. **Dùng làm thương hiệu** — dùng hình Mint hoặc tên "Mint" làm
   logo, avatar, tên sản phẩm, tên thương hiệu của bạn.
6. **Huấn luyện AI** — thu thập / trích xuất ảnh, video, hoặc tham
   số vẽ mặt Mint (kể cả từ `assets/mint-face.json`) để huấn luyện
   mô hình AI/ML, tạo LoRA, fine-tune; trọng số mô hình suy ra từ
   tài sản Mint được coi là phái sinh và chịu cùng điều kiện này.

Phạm vi cấm tên "Mint": chỉ áp dụng khi dùng tên đó trong ngữ cảnh
liên quan đến nhân vật / trợ lý AI Mimint (tên nhân vật, tên sản
phẩm dùng hình Mint, thương hiệu gắn với nhân vật). Dùng từ "mint"
nói chung (bạc hà, hoặc tên khác không liên quan đến nhân vật)
không thuộc điều kiện này.

Không có "cho phép ngầm": việc code được cấp phép Apache 2.0, việc
repo công khai, hay việc chủ repo không trả lời, đều KHÔNG phải là
cho phép. Apache 2.0 cho phép bạn sửa code — nhưng không tự động
cho phép bạn sửa thiết kế nhân vật, kể cả sửa rồi dùng riêng.

## 3. Cách xin phép

1. Mở một issue trên https://github.com/ArtyormSatori/mimint với
   tiêu đề bắt đầu bằng `[character-permission]`.
2. Ghi rõ: tài sản nào bạn muốn dùng, dùng vào việc gì (sản phẩm /
   mục đích), trong bao lâu, có chỉnh sửa không, có thương mại không.
3. Chờ trả lời bằng văn bản trong issue (hoặc bằng văn bản khác từ
   chủ repo). Chỉ văn bản đó mới là cho phép; im lặng không phải là
   cho phép.
4. Khi được phép, tuân thủ đúng phạm vi đã ghi trong văn bản cho
   phép; ngoài phạm vi đó vẫn cấm.
5. Quyền được cấp là không độc quyền, không chuyển nhượng, và có thể
   thu hồi — trừ khi văn bản cho phép ghi khác.

## 4. Ví dụ ĐƯỢC (không cần xin phép)

- Xem hình Mint trong repo, chạy app Mimint và thấy mặt avatar Mint
  trong chính app này.
- Fork code Mimint theo Apache 2.0 (fork code được; tài sản nhân
  vật trong fork vẫn chịu điều kiện này). Lưu ý: bấm Fork trên
  GitHub để tạo repo công khai LÀ phân phối lại — muốn fork công
  khai còn hình Mint thì phải xin phép TRƯỚC khi bấm Fork. Các cách
  hợp lệ: fork riêng tư (không công khai), hoặc gỡ tài sản nhân vật
  khỏi cây trước khi fork công khai (xóa `assets/mint-thinking.*`,
  `assets/mimint-thinking.mp4` và `assets/mint-face.json`; mặt avatar sẽ hiển thị trung tính
  theo bộ mặc định).
- Chụp ảnh màn hình app Mimint (có hình Mint) để review, báo lỗi,
  hoặc viết tài liệu về chính Mimint — chỉ trong ngữ cảnh đó; không
  được crop riêng nhân vật ra dùng độc lập, và không dùng ảnh chụp
  làm ảnh quảng bá, ảnh bìa cho sản phẩm, sách hay khóa học khác.
- Viết code mới dùng API / emotion code của Mimint mà không sao chép
  thiết kế khuôn mặt.

## 5. Ví dụ KHÔNG ĐƯỢC (khi chưa có văn bản cho phép)

- Lấy ảnh Mint làm avatar cho sản phẩm, tài khoản hay thương hiệu
  khác.
- Sửa một look của Mint (đổi màu, đổi mặt) rồi phát hành bản sửa.
- Nhúng video `mimint-thinking.mp4` vào quảng cáo / landing page của
  sản phẩm khác.
- Bán merchandise (áo, sticker, figure…) dùng hình Mint hoặc tên
  "Mint".
- Tạo nhân vật "em gái / phiên bản khác của Mint" dựa trên thiết kế
  này.
- Fork repo rồi phát hành lại nguyên bộ tài sản nhân vật như của
  mình.

## 6. Quan hệ với Apache License 2.0

Repo này là "Apache 2.0 with additional conditions": code theo
Apache 2.0, tài sản nhân vật theo điều kiện riêng này. Điều kiện
riêng không làm thay đổi quyền của bạn đối với code, và Apache 2.0
không mở rộng quyền của bạn đối với tài sản nhân vật. Nếu bạn phân
phối lại code (kể cả fork đã gỡ tài sản nhân vật), bạn vẫn phải giữ
`LICENSE` và `NOTICE` theo điều 4 của Apache 2.0.

---

# Mint Character Licence (English translation, for reference)

## 1. Scope

"Mint" is the AI assistant character of the Mimint project: its
shape, colours, face, the name "Mint" as the name of this
character, and every image / video / animation depicting it.

Character assets in this repository (verified at commit `dd883e0`):

| Asset | Location in the repo |
|---|---|
| Thinking image and video of Mint | `assets/mint-thinking.gif`, `assets/mint-thinking.jpg`, `assets/mimint-thinking.mp4` |
| Avatar face — 9 states: Neutral, Happy, Listening, Thinking, Speaking, Surprised, Confused, Sleepy, Celebrating | Design data file: `assets/mint-face.json` — eye/mouth shapes, colours (blush), tilt, bounce, and star burst for the 9 states. The generic rendering code reads the parameters from that file (`crates/mimint-ui/src/face.rs` + `face_design.rs`, Apache 2.0); the 9 emotion codes (`EmotionCode` in `crates/mimint-core/src/emotion.rs`) are generic identifiers, not the design. No per-state image files. A neutral default parameter set (a generic face, not Mint's) lives in code (`FaceDesign::neutral`, Apache 2.0) so a fork can replace the design immediately by deleting the design file. |
| The character name "Mint" | The name attached to the character throughout the repo |

NOT in this repository (same conditions apply when published; do
not mistake them for repo contents): the 20 looks
(`assets/looks/` does not exist in the repo) and the design sources
(`design/`). Those are local materials only at this commit. The
character's TTS voice files are NOT character assets under these
conditions: the current voice is a machine TTS voice, not a
personal voice. A "personal voice" here means the voice of a real
person, or a voice created specifically for this project (a hired
voice actor / a voice fine-tuned on a real person's voice) — this
will be revisited only if a personal voice exists. The current TTS
voice files are not in this repository and are not distributed with
the product.

The generic rendering code that draws the avatar face (reading the
parameters from the design file and drawing the shapes), the
technical docs, tests and build scripts are NOT in this scope —
they are under the Apache License 2.0 (see `LICENSE`). The line is:
code is Apache 2.0; the face design parameters in
`assets/mint-face.json` (eye/mouth shapes, colours, the 9 core face
states) are under these additional conditions. Fixing logic/bugs in
the rendering code is allowed; changing the shape parameters in the
design file is not (without prior written permission).

SPDX note: the code is `Apache-2.0` (the standard SPDX
identifier); these additional conditions for the character have no
SPDX identifier — they are additional terms on top of SPDX
(recorded as `LicenseRef-Mimint-Character-1.0`).

## 2. Prohibited without prior written permission

Without prior written permission from the repository owner, you
may NOT, with respect to Mint's character assets:

1. **Modify** — change the shape, colours, face, name, or any
   detail of the character (including private, unpublished edits).
2. **Reuse** — reuse the images / video / avatar face / name
   "Mint" in any other product, app, website, article, video or
   document, including **embedding them in another product**.
3. **Redistribute** — repost, reshare, or include the character
   assets in any distribution (including a fork that keeps the
   assets as-is).
4. **Create derivatives** — create a new character, new look,
   variant, commercial fan art, or any work based on Mint.
5. **Use as branding** — use Mint's image or the name "Mint" as
   your logo, avatar, product name or brand name.
6. **Train AI** — collect / extract Mint's images, video, or
   face-drawing parameters (including from `assets/mint-face.json`)
   to train an AI/ML model, create a LoRA, or fine-tune; model
   weights derived from Mint's assets count as derivatives and are
   under these same conditions.

Scope of the ban on the name "Mint": it applies only when the name
is used in a context related to the character / the Mimint AI
assistant (the character's name, a product name using Mint's image,
a brand tied to the character). Using the word "mint" in general
(the herb, or an unrelated name) is not covered by these
conditions.

There is no implied permission: the code being under Apache 2.0,
the repository being public, or the owner not replying, is NOT
permission. The Apache License 2.0 lets you modify the code — but
it does not by itself let you modify the character design,
including private, unpublished edits.

## 3. How to ask for permission

1. Open an issue at https://github.com/ArtyormSatori/mimint with
   a title starting with `[character-permission]`.
2. State clearly: which assets you want to use, for what (product
   / purpose), for how long, whether you will modify them, and
   whether the use is commercial.
3. Wait for a written reply in the issue (or another written reply
   from the repository owner). Only that written reply counts as
   permission; silence is not permission.
4. If permission is granted, stay within the scope stated in that
   written permission; anything outside it remains prohibited.
5. Permission, when granted, is non-exclusive, non-transferable,
   and revocable — unless the written permission states otherwise.

## 4. Allowed without asking (examples)

- Viewing Mint's images in the repo, running the Mimint app and
  seeing Mint's avatar face inside that app.
- Forking the Mimint code under Apache 2.0 (forking the code is
  allowed; the character assets in the fork remain under these
  conditions). Note: pressing Fork on GitHub to create a public
  repository IS redistribution — to make a public fork that still
  shows Mint, ask for permission BEFORE pressing Fork. Valid
  alternatives: a private fork (not public), or removing the
  character assets from the tree before forking publicly (delete
  `assets/mint-thinking.*`, `assets/mimint-thinking.mp4` and `assets/mint-face.json`; the avatar
  face then shows the neutral default).
- Taking screenshots of the Mimint app (showing Mint) to review,
  report a bug, or document Mimint itself — only in that context;
  do not crop the character out for standalone use, and do not use
  screenshots as promotional or cover images for another product,
  book, or course.
- Writing new code that uses Mimint's API / emotion codes without
  copying the face design.

## 5. Not allowed without prior written permission (examples)

- Using Mint's image as the avatar of another product, account or
  brand.
- Editing a Mint look (changing colours, changing the face) and
  releasing the edit.
- Embedding the `mimint-thinking.mp4` video in another product's
  advertisement / landing page.
- Selling merchandise (shirts, stickers, figures, …) using Mint's
  image or the name "Mint".
- Creating a "sister / alternate version of Mint" character based
  on this design.
- Forking the repository and redistributing the character assets
  as your own.

## 6. Relationship with the Apache License 2.0

This repository is "Apache 2.0 with additional conditions": code
under Apache 2.0, character assets under these additional
conditions. These conditions do not change your rights over the
code, and the Apache License 2.0 does not extend your rights over
the character assets. If you redistribute the code (including a
fork with the character assets removed), you must still keep
`LICENSE` and `NOTICE` as required by section 4 of the Apache
License 2.0.

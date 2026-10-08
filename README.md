# AntiSwitcher

> Hệ thống Quản lý Tài khoản AI & Proxy Protocol chuyên nghiệp (v4.1.22)

<div align="center">
  <img src="public/icon.png" alt="Antigravity Logo" width="120" height="120" style="border-radius: 24px; box-shadow: 0 10px 30px rgba(0,0,0,0.15);">

  <h3>Cổng Gateway AI Hiệu năng Cao Cá Nhân</h3>
  <p>Proxy Gemini & Claude một cách liền mạch. Tương thích OpenAI. Bảo Mật Là Ưu Tiên.</p>

  <p>
    <a href="https://github.com/lbjlaq/Antigravity-Manager">
      <img src="https://img.shields.io/badge/Phiên_bản-4.1.22-blue?style=flat-square" alt="Phiên bản">
    </a>
    <img src="https://img.shields.io/badge/Tauri-v2-orange?style=flat-square" alt="Tauri">
    <img src="https://img.shields.io/badge/Backend-Rust-red?style=flat-square" alt="Rust">
    <img src="https://img.shields.io/badge/Frontend-React-61DAFB?style=flat-square" alt="React">
    <img src="https://img.shields.io/badge/Giấy_phép-CC--BY--NC--SA--4.0-lightgrey?style=flat-square" alt="Giấy phép">
  </p>
</div>

---

**AntiSwitcher** là ứng dụng desktop all-in-one được thiết kế cho nhà phát triển và người đam mê AI. Kết hợp hoàn hảo giữa quản lý đa tài khoản, chuyển đổi protocol, và lập lịch request thông minh để cung cấp cho bạn một **Trạm Relay AI Cục Bộ** ổn định, tốc độ cao và chi phí thấp.

## Tính năng chính

### Quản lý Tài khoản AI Chuyên nghiệp
- **OAuth 2.0 (Tự động/Thủ công)**: Tạo URL xác thực có thể sao chép để hoàn thành auth trong bất kỳ trình duyệt nào
- **Đa chiều nhập**: Hỗ trợ nhập token đơn lẻ, nhập hàng loạt JSON, và di chuyển tự động từ database V1
- **Giám sát thời gian thực**: Theo dõi quota Gemini Pro, Gemini Flash, Claude và tạo hình ảnh Gemini
- **Phát hiện 403 tự động**: Đánh dấu và bỏ qua các tài khoản có bất thường quyền

### Chuyển đổi Protocol & Relay (API Proxy)
- **OpenAI Format**: Endpoint `/v1/chat/completions`, tương thích với 99% ứng dụng AI
- **Anthropic Format**: Interface `/v1/messages` gốc, hỗ trợ mọi tính năng của **Claude Code CLI**
- **Gemini Format**: Gọi trực tiếp từ Google AI SDK chính thức
- **Tự phục hồi thông minh**: Retry tự động mức mili-giây khi gặp lỗi 429/401

### Model Router
- ** ánh xạ theo Series**: Phân loại model IDs phức tạp thành "Series Groups"
- **Regex tùy chỉnh**: Ánh xạ model cấp độ chuyên gia cho kiểm soát chính xác
- **Routing theo Tier**: Ưu tiên tự động dựa trên tier tài khoản (Ultra/Pro/Free)
- **Hạ cấp ngầm**: Nhận diện tác vụ nền và chuyển hướng đến Flash models

### Multimodal & Imagen 3
- Kiểm soát hình ảnh nâng cao với thông số OpenAI `size`
- Payload lên đến **100MB**

## Cài đặt

### Cài đặt Terminal (Khuyến nghị)

**Linux / macOS:**
```bash
curl -fsSL https://raw.githubusercontent.com/lbjlaq/Antigravity-Manager/v4.1.22/install.sh | bash
```

**Windows (PowerShell):**
```powershell
irm https://raw.githubusercontent.com/lbjlaq/Antigravity-Manager/main/install.ps1 | iex
```

### macOS - Homebrew
```bash
brew tap lbjlaq/antigravity-manager https://github.com/lbjlaq/Antigravity-Manager
brew install --cask antigravity-tools
```

### Docker
```bash
docker run -d --name antigravity-manager \
  -p 8045:8045 \
  -e API_KEY=sk-your-api-key \
  -e WEB_PASSWORD=your-login-password \
  -v ~/.antigravity_sw:/root/.antigravity_sw \
  lbjlaq/antigravity-manager:latest
```

## Cách sử dụng nhanh

### Với Claude Code CLI
```bash
export ANTHROPIC_API_KEY="sk-antigravity"
export ANTHROPIC_BASE_URL="http://127.0.0.1:8045"
claude
```

### Với Python
```python
import openai

client = openai.OpenAI(
    api_key="sk-antigravity",
    base_url="http://127.0.0.1:8045/v1"
)

response = client.chat.completions.create(
    model="gemini-3-flash",
    messages=[{"role": "user", "content": "Hello"}]
)
print(response.choices[0].message.content)
```

## Ủng hộ

Nếu bạn thấy dự án này hữu ích, hãy mua cho tôi một ly cà phê!

<a href="https://www.buymeacoffee.com/Ctrler" target="_blank"><img src="https://cdn.buymeacoffee.com/buttons/v2/default-green.png" alt="Buy Me A Coffee" style="height: 60px !important; width: 217px !important;"></a>

## Giấy phép & Bảo mật

- **Giấy phép**: **CC BY-NC-SA 4.0**. Chỉ dùng cho mục đích phi thương mại.
- **Bảo mật**: Tất cả dữ liệu tài khoản được mã hóa và lưu trữ cục bộ trong database SQLite. Dữ liệu không bao giờ rời khỏi thiết bị của bạn.

---

<div align="center">
  <p>Nếu bạn thấy công cụ này hữu ích, hãy cho nó một ⭐️ trên GitHub!</p>
  <p>Copyright © 2025 Antigravity Team.</p>
</div>

## Cập nhật

- **v6.0.5**
- Add folder tự upload dự án sau khi kiểm tra trạng thái workspace.
- Upload tôn trọng .gitignore ở thư mục gốc và thư mục con, bao gồm rule ! giữ lại file.
- Bỏ qua dependency, build output và cache phổ biến; các file bị bỏ qua không làm lệch trạng thái sync.
- Yêu cầu cập nhật server bằng patches/jobautopc-workspace-ignore.patch trước khi dùng sync ở phiên bản này.


Porty v0.1.2 — 原生 SSH 端口转发 + 远程端口自动发现
=====================================================

纯 Rust + GPUI，原生 GPU 渲染，无 Electron。

【Windows x64】
  1. 解压后双击 porty.exe。
  2. 若 SmartScreen 拦截：更多信息 → 仍要运行（未签名构建的正常提示）。
  3. 需要 Windows 10 1703+ / Windows 11 x64。

【macOS Apple Silicon】
  1. 解压后把 Porty.app 拖到“应用程序”。
  2. 首次启动请右键 Porty.app → 打开；如果提示“已损坏”，在终端执行：
         xattr -dr com.apple.quarantine /Applications/Porty.app
     然后再次打开。
  3. 需要 macOS 11+（arm64）。

【首次使用】
  • 自动导入 ~/.ssh/config 中的主机（Windows 为 %USERPROFILE%\.ssh\config）。
  • 没有配置？点左侧 HOSTS 标题栏的 “+” 手动添加主机。
  • 认证优先级：系统 SSH Agent（Windows OpenSSH Authentication Agent 服务 /
    Pageant、macOS ssh-agent）→ ~/.ssh/id_ed25519、id_rsa 等私钥 → 密码。
  • 密码只保存在内存中，不会写入磁盘。

【常用操作】
  • 选中主机 → Connect。
  • 端口过滤：Cmd/Ctrl+F（或 /）。
  • 快捷转发：Cmd/Ctrl+N，然后输入：
        3000                  → 远程 127.0.0.1:3000 转发到本机 3000
        8080:3000             → 远程 3000 转发到本机 8080
        10.0.0.2:5432:15432   → 远程 10.0.0.2:5432 转发到本机 15432
        10.0.0.2:5432->15432  → 同上
  • 本机端口被占用时会自动用下一个可用端口；端口会变为琥珀色。
  • VISIBILITY 开关切到 LAN 会绑定 0.0.0.0（Tailscale/局域网访问）。
  • 每行操作：Open in browser、Copy、Pause/Resume、Remove。
  • Cmd/Ctrl+Enter 连接/断开；Cmd/Ctrl+R 重新导入 ssh config；
    Cmd/Ctrl+Shift+L 切换深色/浅色主题。

【自动发现】
  • 主机选中后可在侧栏打开 Auto-discover ports，连接后每 3 秒轮询远端
    Linux 的 /proc/net/tcp 和 ss -tlpn。
  • 打开 Auto-forward new 后，新端口会自动建立转发。
  • 目前仅支持 Linux 远端（依赖 /proc 和 ss）。

【文件位置】
  • Windows 设置：%APPDATA%\Porty\settings.json
  • macOS 设置：~/Library/Application Support/Porty/settings.json
  • 重置：关闭程序后删除该 settings.json。

【日志 / 崩溃排查】
  • 日志文件：
      Windows: %APPDATA%\Porty\logs\porty.log
      macOS:   ~/Library/Application Support/Porty/logs/porty.log
  • 程序会把 panic 信息（位置 + backtrace）写入这个文件。
  • 需要更详细日志时，在终端设置 RUST_LOG=debug 后启动：
      PowerShell:  $env:RUST_LOG="debug"; .\porty.exe
      macOS:       RUST_LOG=debug /Applications/Porty.app/Contents/MacOS/porty

【已知限制】
  • 不支持 ProxyJump（会读取但不会使用该配置）。
  • 密码不会持久化，重启后需要重新输入。
  • 二进制未签名/未公证，仅建议个人使用。

图标来自 Lucide（ISC）。本程序以 MIT 许可证发布。

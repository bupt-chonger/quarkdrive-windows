# QuarkDrive Windows

参考 [bupt-chonger/QuarkDrive](https://github.com/bupt-chonger/QuarkDrive) 的夸克 API 适配，在 Windows 10 1709+ 使用原生 **Cloud Files API（CFAPI）** 把夸克网盘显示在资源管理器中。

## 功能概览

- 原生注册 Windows 同步根，不需要 Dokan、WinFsp 或盘符驱动；
- 按需列出夸克目录并创建 Windows 占位文件；
- 打开文件时使用夸克下载地址和 HTTP Range 分段取数；
- 支持 Windows 文件名转义、远端稳定 `fid`、文件大小与时间元数据；
- 使用夸克网盘 Web 二维码登录，自动保存登录会话和账号信息；
- 提供目录列表、挂载状态与安全注销命令；
- 监视挂载目录中新建的文件夹和文件，自动创建远端目录、上传文件并转换为占位符；
- 同步资源管理器中的重命名、移动和删除操作；
- 设置页提供 API 日志、返回数量、耗时和可释放空间选择。

> 夸克未提供供本项目使用的稳定公开文件 API。本项目统一使用夸克网盘 Web 登录会话调用接口；接口可能因夸克改版、风控或账号策略失效。请勿把配置文件提交进 Git。

## 环境要求

- Windows 10 1709 / Windows 11，挂载目录所在磁盘必须为 NTFS；
- Rust 1.85+（项目使用 Rust 2024 edition）；
- 可使用夸克网盘 APP 扫码登录的账号。

配置文件保存在当前用户 `%APPDATA%\QuarkDrive\config.json`，运行日志保存在 `%LOCALAPPDATA%\QuarkDrive\quarkdrive.log`。程序只使用夸克网盘网页端会话，不会通过第三方服务交换登录令牌。

## 使用

程序挂载后会常驻 Windows 任务栏通知区域：

- 双击夸克图标：打开设置
- 右键夸克图标：打开网盘、设置或退出
- 设置页面：修改挂载位置、资源管理器显示名称及开机启动；切换挂载位置时会先确认，随后删除旧目录中的本地文件和文件夹，并在新目录重新同步云端内容
- 账号状态卡：点击“二维码登录”，扫码后自动展示昵称、账号 ID 并保存登录会话
- 云盘根目录：可在“全部文件”和账号顶层目录之间选择挂载范围
- 本地空间：检测网盘逻辑容量、实际本地占用和可释放容量；确认后释放未固定文件的本地缓存，云端内容和资源管理器占位符保持不变

资源管理器同步根、桌面快捷方式和任务栏通知区域均使用 Quark 品牌图标。

### 直接安装

双击仓库根目录的 `安装夸克网盘.cmd`。它会：

- 安装到 `%LOCALAPPDATA%\Programs\QuarkDrive\quarkdrive.exe`；
- 创建桌面“夸克网盘”快捷方式；
- 启动程序，首次运行会打开设置页扫码登录。

本机安全策略会拒绝从含中文的工程路径直接执行 EXE，因此发布程序需要先复制到上述纯英文安装目录。

```powershell
cargo build --release

# 最简单的方式：直接运行，首次启动会打开设置页扫码登录。
cargo run --release

# 也可以明确指定首次挂载位置：
cargo run --release -- init --cookie '完整 Cookie' --mount "$env:USERPROFILE\QuarkDrive" # 兼容命令行方式

# 先检查登录会话与远端根目录
cargo run --release -- doctor

# 已登录时可单独执行挂载；保持此进程运行
# 挂载成功后程序会自动打开资源管理器，左侧导航栏也会出现“夸克网盘”。
cargo run --release -- mount

# 检查资源管理器注册状态和实际挂载路径
cargo run --release -- status

# 不再使用时注销同步根（不会删除本地目录或文件）
cargo run --release -- unregister
```

列出根目录但不挂载：

```powershell
cargo run --release -- list
```

使用其他配置文件时，在子命令前或后传入 `--config C:\path\config.json`。

## 配置格式

```json
{
  "cookie": "由二维码登录自动保存",
  "account_nickname": "夸克账号昵称",
  "account_id": "夸克账号 ID",
  "account_avatar": "",
  "remote_root_id": "0",
  "remote_root_name": "夸克网盘",
  "mount_path": "C:\\Users\\name\\QuarkDrive"
}
```

`remote_root_id` 可改成夸克目录 `fid`，把某个子目录作为挂载根。

设置页二维码登录：

```powershell
cargo run --release
```

在资源管理器设置页点击“二维码登录”，使用夸克网盘 APP 扫码即可。登录成功后，程序从夸克网盘返回值读取账号信息并自动保存会话；切换账号时重复扫码。

## 验证

```powershell
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

仓库路径含中文时，部分 Windows 安全策略会拒绝执行 Cargo 构建脚本，因此 `.cargo/config.toml` 将编译产物放到 `C:\Users\admin\quarkdrive-windows-target`。如果换了 Windows 用户，请将它改成该用户有写权限的纯 ASCII 路径。

## 后续生产化清单

1. 使用 Windows Credential Manager 加密保存二维码登录会话；
2. SQLite 持久化传输队列、远端版本、冲突副本与崩溃恢复；
3. 远端变更轮询、断网重试、限流与端到端集成测试。

## 安全边界

- `unregister` 只注销 CFAPI 同步根，不删除本地内容；
- 文件关闭回调只在 CFAPI 报告存在未同步本地修改时上传，避免读取云端文件时重复上传；
- 删除回调会先提交夸克网盘删除请求，远端请求失败时本地删除会被拒绝；重命名和移动通过对应远端 API 同步；
- 当前仍缺少持久化队列、断网重试和完整冲突处理，请不要把挂载目录作为唯一文件副本。

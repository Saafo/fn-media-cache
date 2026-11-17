# FN Media Cache

飞牛影视本地缓存服务。通过本地代理连接飞牛影视，提前缓存影片或后续剧集，继续使用原有播放器播放。

Rust 后台可以独立运行，管理页面内嵌在程序中。Tauri 桌面入口使用同一个后台，关闭桌面窗口不影响已托管的服务。

## 当前能力

- HTTP 代理与单文件 Range 缓存，支持断点续传、暂停和恢复；文件未下载完成时，也可读取已缓存部分，缺失部分回源。
- 取消任务、删除本地缓存、调整等待队列顺序；查看单片本地与回源读取量、下载速度和预计剩余时间。
- 搜索媒体库、浏览季集并选择下载；可配置后续剧集预取、限速与播放时让行。
- 同一服务器的多地址测速、下载优选与失败切换。
- 缓存容量管理、已看内容延时清理及手动保留。
- 自动重试、同账号会话恢复、轮换日志。
- 无痕模式拦截已适配的观看记录写入。

目前构建与测试覆盖 macOS Apple Silicon。Windows 运行及桌面托管仍待验证；HLS、DASH、转码和跨域视频下载暂未支持。

## 运行后台

```sh
cargo build --release --locked
./target/release/fn-media-cache init --upstream https://media.example.com
./target/release/fn-media-cache serve
```

把示例地址换成自己的飞牛影视服务器。`init` 生成本地 `fn-media-cache.toml`，不会覆盖已有文件；也可复制 `config.example.toml` 后修改。配置文件不要提交到仓库。

默认代理地址为 `http://127.0.0.1:28005`，管理地址为 `http://127.0.0.1:28006`。在管理页输入本机 `data/admin-token` 中的令牌，再让播放器连接代理地址并正常登录。

需要局域网播放器连接时，把配置中的 `proxy_listen` 改为 `0.0.0.0:28005`。管理接口仍仅允许本机访问。

## 配置与使用

同一服务器的备用地址可以放进 `upstream_alternates`。地址必须属于同一台服务器，登录凭据会发送到这些地址。

取消任务会保留已下载的数据，并停止自动续跑；可以手动继续。删除缓存会停止任务并释放本地文件，正在使用的文件会等读写结束后清理。调整队列只影响等待中的任务，不打断当前下载。

播放读取统计按影片汇总本次后台运行期间的本地与回源字节，重启后清空；无痕模式不记录这项统计。缓存仍需向上游验证权限和文件版本，不提供离线鉴权。

自动预取默认关闭。预取集数、下载速率、缓存上限、看完后的保留时间、重试与日志参数都可配置。相关设置保存在本机，重启后保留。清理只删除本机缓存，不修改上游影片或观看记录。

缓存数据、登录会话和管理令牌只保存在本机。运行日志位于数据目录的 `logs/` 下，包含 `service.log` 和桌面操作的 `desktop.log`。分享日志前仍应检查内容。

无痕模式不会删除已有观看历史，也不提供网络匿名；客户端可能在关闭无痕后补报自身缓存的进度。

## 构建桌面入口

先构建后台并放到桌面打包所需的位置：

```sh
mkdir -p bin
cp target/release/fn-media-cache bin/fn-media-cache-macos-arm64
cd desktop
npm ci
npm run tauri build -- --bundles app
```

桌面入口目前支持 macOS 的用户级后台托管。新安装使用应用自己的数据目录；已有旧版配置和数据可继续读取。

## 开发验证

```sh
cargo fmt --check
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
python3 tests/integration.py --binary target/release/fn-media-cache
python3 tests/native_integration.py --binary target/release/fn-media-cache
python3 tests/routes_integration.py --binary target/release/fn-media-cache
python3 tests/automation_integration.py --binary target/release/fn-media-cache
python3 tests/privacy_integration.py --binary target/release/fn-media-cache
python3 tests/recovery_integration.py --binary target/release/fn-media-cache
python3 tests/task_playback_integration.py --binary target/release/fn-media-cache
python3 tests/shutdown.py --binary target/release/fn-media-cache
```

这些测试使用临时目录、虚构凭据和本机模拟服务器，不需要真实影视账号。模拟测试不代表所有飞牛版本、播放器或真实网络环境均已通过验证。

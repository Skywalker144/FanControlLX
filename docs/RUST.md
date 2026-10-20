# Rust 后台（0.6）

控制服务和独立 NVIDIA 写入进程均由 `target/release/fancontrollx` 提供。HTTP 服务仅监听回环地址，使用两个固定请求线程和有界队列；读取请求体不持有控制锁。网页资源编译进可执行文件。

## 模块

| 模块 | 职责 |
|---|---|
| `src/config.rs` | v1/v2 配置校验、首次迁移备份、原子保存 |
| `src/engine.rs` | 曲线、平滑、保护、识别测试、控制策略及网页操作 |
| `src/hardware.rs` | hwmon 发现缓存与实时采样、主板控制、NVML 读数 |
| `src/nvml.rs` | 动态加载可选的 NVIDIA 驱动库，封装 C ABI |
| `src/writer.rs` | 独立 NVIDIA 写入进程、逐通道租约和恢复记录 |
| `src/server.rs` | 本地 HTTP、Host/Origin/CSRF 检查、完整与增量状态 |

代码使用标准库线程和套接字；依赖锁定在 `Cargo.lock`。没有 NVIDIA 驱动时主板仍可运行。只有 NVML 无法提供 GPU 温度时才尝试 `nvidia-smi`，成功采样间隔 3 秒，失败退避 30 秒，子进程超时 2 秒。

## 采样和写入

启动时发现设备，手动扫描会立即刷新。可见编辑页每 5 秒左右发送心跳，后台持有 15 秒编辑租约，仅在租约有效时每 30 秒刷新拓扑。观看页及正常无网页后台不做周期性拓扑发现。读数失效使缓存失效，按 30 秒间隔重试发现；已配置通道缺失也会触发恢复发现，因而设备重连不依赖编辑页。失效读数立即用于保护判断，不会用旧温度填充。NVML 范围最多缓存 60 秒，重新发现设备时清空。句柄只在其所属 NVML 实例生命周期内使用。

温度、测速、实时模式和实际输出仍约每秒读取。过热和所选温度源缺失继续绕过曲线平滑、回差、响应时间和用户输出上限。

主板输出变化立即写入；不变时每 3 秒重新写入。看门狗独立按芯片每 3 秒续期，不能因输出稳定而停止续期。NVIDIA 每轮继续发送有效控制请求，写入进程独立检查自身温度并更新 10 秒租约；不能以缓存替代安全检查。

网页不可见时暂停轮询，恢复可见时立即取状态。后台控制与网页连接无关。只刷新可见标签页；`/api/state?since=<config_revision>&after=<timestamp>` 可省略未变的配置并返回增量历史，原 `/api/state` 保持完整响应。

0.6.1 起默认 `/`（也可用 `/view`）为观看页，使用只读 `/api/view` 返回缓存的实时状态，不发送配置、CSRF 令牌、历史、设备发现或日志。`/edit` 使用原编辑工作台。两页共用同一后台和端口，不额外创建控制进程。编辑心跳必须通过原有 CSRF 校验；观看请求不延长发现租约。编辑页隐藏、关闭或崩溃后停止发送心跳，后台的发现租约在最后一次收到心跳的 15 秒后失效。页面分离是防误操作和减少开销，不是用户权限隔离；本机用户仍可打开编辑页。

## 构建和升级

需要 Rust 1.82+ 和系统 C 链接器。使用 `./build.sh`（等价于 `cargo build --release --locked`）。构建结果可在兼容的 Linux 系统运行；仍需要该系统的硬件驱动及 NVIDIA 驱动库（若使用显卡）。

首次从 Python 服务升级：

```bash
./build.sh
sudo ./install-service.sh
```

沿用系统驱动的其他机器使用 `--skip-driver`。安装器在停止旧服务之前验证可执行文件及驱动，备份原服务单元和配置，保留原控制开关。安装器由同一 Rust 二进制的 `--install-service` 子命令提供，不常驻；Shell 入口只定位项目目录并转交参数。

随后每次更新：

```bash
git pull --ff-only
./build.sh
sudo systemctl restart fancontrollx
```

网页资源也需要重新构建，刷新浏览器即可加载新资源。

## 验证与比较

```bash
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
python3 -m unittest discover -s tests -v
node --check static/app.js
```

`tests/test_rust.py` 在临时目录运行演示服务，并编译 `tests/mock_nvml.c` 为测试用驱动库，验证独立进程的 EOF、SIGTERM、SIGKILL 恢复、超时租约、原手动目标恢复和显卡自身温度保护。不会通过模拟库操作真实显卡。

只读硬件采样：

```bash
./target/release/fancontrollx --benchmark 30
```

正在运行的整个服务（包括 GPU 子进程）：

```bash
python3 tests/benchmark_service.py --label rust --output test-results/performance/rust-service.json
```

默认三个 30 秒窗口，直接读取 cgroup CPU 和内存统计。100% CPU 表示占满一个核心，内存不重复计算父子进程共享页。窗口内不主动请求网页接口；窗口后另测完整状态接口延迟。比较时应保持配置、启用通道、显卡模式、网页连接数量和机器负载一致。短时数据不等于长期功耗，也不能用于推算节电瓦数。

本机迁移前后结果和原始记录见 [性能对比](PERFORMANCE.md)。对照旧版只读采样时，从原始提交导出独立目录，再运行 `tests/benchmark_sampling.py`；不要使用已经修改的工作区作为旧版基线。

旧 Python 应用包及只针对旧实现的测试已删除。对应行为覆盖迁入 Rust 单元测试和 `tests/test_rust.py`；测试说明见 [TESTING.md](TESTING.md)。性能测量脚本保留，通过独立的旧提交导出目录获取 Python 基线。不要同时启动两个具有硬件控制权限的后台。

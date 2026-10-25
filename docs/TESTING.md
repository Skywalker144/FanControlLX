# Rust 运行链与测试迁移

应用和安装运行链已不再包含 Python。`install-service.sh` 调用 Rust 安装子命令；systemd 的主服务和 NVIDIA 恢复钩子调用同一二进制。保留的 Python 文件只有：

- `tests/test_rust.py`：启动临时 Rust 演示服务和模拟 NVML 子进程的黑盒测试。
- `tests/benchmark_service.py`：读取当前服务的 cgroup 和 API。
- `tests/benchmark_sampling.py`：对比当前 Rust 与从历史提交导出的 Python 只读采样。

旧 `fancontrollx/` 包及直接导入该包的 `test_core.py`、`test_workspace.py`、`test_nvidia.py` 已移除。相同需求的重复测试合并为针对实际 Rust 实现的行为测试，不沿用旧测试数量作为覆盖率指标。

| 原测试职责 | 当前覆盖 |
|---|---|
| 默认曲线、插值、输出范围、平均/回差/延迟/升降速限制 | `src/tests.rs` 的计算及策略测试 |
| 原始温度过热、缺失温度、主板停转保护、GPU 自身温度及零转速模式 | `src/tests.rs` + 模拟 NVML 进程测试 |
| 接管初始输出、保留原租约、停止恢复、恢复失败重试 | `src/tests.rs` |
| 后台重启、延迟出现设备、前台不自动接管、停止全部持久化 | `src/tests.rs` + `tests/test_rust.py` |
| 命名确认、临时识别、超时、单通道测试、泵保护、恢复原控制 | Rust 行为测试、真实子进程超时测试及浏览器测试 |
| 共享曲线、局部修改、并发冲突、排序、原子分配、磁盘保存失败 | Rust 单元、HTTP 黑盒和浏览器回归 |
| v1 原文件备份、损坏配置保留、参数校验 | Rust 配置测试及黑盒测试 |
| hwmon 原状态恢复、设备重编号、未知芯片只读 | 临时 sysfs 目录测试 |
| NVIDIA 写入前持久化、逐通道超时、旧令牌、恢复失败、EOF/SIGTERM/SIGKILL | Rust Writer 测试 + `tests/mock_nvml.c` 黑盒进程测试 |
| 安装预检、驱动检查、备份、配置开关、服务路径转义、前台锁冲突 | `src/install.rs` 中使用临时目录和命令记录器的测试 |
| HTTP Host/Origin/令牌、路径限制、增量响应 | Rust Host 测试 + `tests/test_rust.py` |
| 观看/编辑分离、隐藏页停止轮询、手机布局 | API 测试 + `tests/browser-smoke.mjs` |
| 健康后台不定期发现、编辑租约超时、故障发现退避、实时读取 | Rust 调度测试及临时 sysfs 测试 |

运行：

```bash
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
./build.sh
python3 -m unittest discover -s tests -v
node --check static/app.js
node --check static/view.js
bash -n install-service.sh webui.sh build.sh
```

控制循环和 hwmon 分配基准默认忽略，显式运行：

```bash
cargo test --release --locked benchmark_ -- --ignored --nocapture --test-threads=1
```

额外测量 NVML 对象复用时，先编译模拟库；下列命令不访问真实显卡：

```bash
mock_dir=$(mktemp -d /tmp/fancontrollx-nvml-bench.XXXXXX)
cc -shared -fPIC -Wall -Wextra -Werror tests/mock_nvml.c -o "$mock_dir/libnvidia-ml.so.1"
LD_LIBRARY_PATH="$mock_dir" FANCONTROLLX_MOCK_DIR="$mock_dir" \
  cargo test --release --locked benchmark_mock_nvml_sampling -- --ignored --nocapture
rm -r -- "$mock_dir"
```

基准使用演示数据和临时 hwmon 文件，不访问真实硬件；只在测试构建中启用计数分配器。输出每轮分配次数、累计申请字节和耗时，不能把申请字节当成常驻内存。

`benchmark_control_cycles` 另测 3/64 路配置下编辑/观看增量响应的构造开销，以及固定样本、每步 0.5 秒、60 秒平滑窗口的策略计算。策略基准不包含硬件 I/O，接口基准不包含网络和序列化。比较修改前后时保持同一基准代码、release 编译及相同运行环境。

新增回归覆盖滑动总和在缺失读数、窗口边界和来源修改后的正确性，以及请求头分块读取、正文保留、大小限制、截断与大小写重复。浏览器回归覆盖编辑到观看页的实际导航、曲线库滚动时创建/释放 SVG 且高度不变、传感器读数改变时保留行和文本节点。

索引回归覆盖设备重排、同数量设备替换、移除、温度分组变化和缺失成员。模拟 GPU 进程回归另外覆盖复用布局中的实时温度、模式、输出、RPM、读数失败与恢复，以及风扇数量变化和整卡消失/重连。浏览器回归验证文本节点复用、稳定输出条不写 DOM，以及大曲线库按焦点/指针展开和失焦收起后的保存行为。

黑盒 GPU 测试需要 C 编译器，动态库只注入测试子进程。安装单元测试不会调用真实 systemctl、安装驱动或修改系统目录。

浏览器回归使用临时演示服务，先验证默认观看页没有编辑控件、没有 POST 请求、不加载编辑脚本，再测试 `/edit` 原有全部工作流；截图保存在 `test-results/`。启动命令见 README。

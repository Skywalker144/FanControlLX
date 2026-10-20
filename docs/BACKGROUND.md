# 后台控制与开机自启

浏览器是配置客户端。Rust 可执行文件 `target/release/fancontrollx --service` 持有传感器采样、曲线计算、PWM 控制、硬件租约及本地 HTTP 配置接口；systemd 管理它的生命周期。没有网页客户端连接时，控制循环仍约每秒运行。

## 安装

先保存网页设置，再在原来的前台启动终端按 Ctrl+C。然后执行：

```bash
cd /home/sky/RL/FanControlLX
./build.sh
sudo ./install-service.sh --enable-confirmed
```

脚本完成以下操作：

1. 检查已签名 `nct6687.signed.ko` 的签名者及内核版本。
2. 检查旧进程锁，不强行终止正在运行的前台控制器。
3. 备份配置到 `data/config.before-service-日期时间.json`，保留名称、曲线和其他参数。
4. 使用 `--enable-confirmed` 时，为已经确认身份并绑定硬件的设备设置 `autostart: true`；未确认设备保持停用。
5. 安装驱动到 `/lib/modules/<当前内核>/updates/fancontrollx/nct6687.ko`，运行 depmod。
6. 安装 `/etc/systemd/system/fancontrollx.service`，执行 daemon-reload、enable、start，再检查本地 API 是否实际就绪。

NVIDIA 或已配置好系统驱动的机器可加 `--skip-driver`，跳过 NCT6687 模块检查、安装和启动前加载，仅安装服务。该选项不会安装 NVIDIA 驱动。显卡控制和恢复说明见 [NVIDIA 接入说明](NVIDIA.md)。

可以先执行 `./install-service.sh --dry-run --enable-confirmed` 查看完整服务配置与计划自动控制的设备，不需要 sudo，也不修改系统。

服务与安装器使用编译后的 Rust 可执行文件，不依赖 Python 或 Conda。配置仍使用项目的 `data/config.json`，无需手动复制。服务在每次启动前通过 modprobe 加载驱动并设置 `msi_fan_brute_force=1`，无需另写 modules-load 配置。

## 日常使用

`./webui.sh` 检测到服务已运行后，只显示 <http://127.0.0.1:8090> 并退出。它不会创建第二个控制器；关闭这个终端不影响服务。若服务已经安装但未运行，会提示启动和查看日志的命令。

远程访问仍通过 SSH 转发：

```bash
ssh -N -L 8090:127.0.0.1:8090 sky@sky-5080
```

断开转发只影响浏览器访问，不停止调速。

每颗风扇卡片的“启用控制”开关决定持久控制策略，切换后立即保存并应用，下次服务启动时恢复；关闭后恢复接管前状态。普通曲线编辑保留现有控制租约，不使其他通道重新跑到 100%。

关闭卡片中的“启用控制”或点击“停止全部控制”会保存停用状态。停止整个 systemd 服务则只停止当前运行，保留策略供下次启动恢复。识别测试仍是限时操作，只临时覆盖该通道的输出，不更改持续控制策略。结束或超时后恢复此前的曲线/手动模式；测试过程中重启服务会恢复保存的策略。

## 管理命令

```bash
systemctl status fancontrollx --no-pager
systemctl is-enabled fancontrollx
sudo journalctl -u fancontrollx -n 50 --no-pager
sudo journalctl -u fancontrollx -f
sudo systemctl restart fancontrollx
sudo systemctl stop fancontrollx
sudo systemctl start fancontrollx
sudo systemctl disable --now fancontrollx
```

更新代码或网页资源后先运行 `./build.sh` 再重启服务。修改服务配置或重新安装驱动时，再运行 `sudo ./install-service.sh`；不带 `--enable-confirmed` 会保留当前逐路启用/停用偏好。

## 恢复行为与限制

- 新发现的风扇默认不自动控制。只有已确认、已绑定且持久设置启用的通道才会恢复。
- 设备暂时离线或不能写入时保留待接管状态，每 10 秒重试，网页显示原因。
- 正常停止尝试恢复原来的硬件控制模式。异常退出后 systemd 等待 20 秒再重启，让当前驱动的 15 秒硬件看门狗有机会恢复固件状态；尚未完成恢复的租约不会被覆盖。
- NVIDIA 使用同一 Rust 可执行文件的独立写入进程和逐通道 10 秒租约，失联、超时或 SIGTERM 时恢复原模式。原状态在首次调速前写入 `data/nvidia-leases.json`；`ExecStopPost` 在进程组被强杀后再尝试恢复，接管前也会恢复残留租约。首次升级到 0.6 请运行安装器更新服务单元；后续代码更新需重新构建并重启。驱动失联时恢复可能失败，日志和恢复记录会保留。
- 模块安装仅覆盖构建时的内核。本版没有 DKMS 自动重建。内核更新后需要重新构建并用同一受信任密钥签名，然后重新运行安装脚本。模块加载失败时，服务启动失败并记录日志，保留硬件自身控制。
- 本次对服务策略的自动化测试使用模拟硬件；真实开机、异常断电、内核故障等系统级行为仍需在本机验证。

## 更新应用

执行 `git pull`、`./build.sh` 后运行 `sudo systemctl restart fancontrollx`，再刷新网页。首次从 Python 后台升级还需运行安装器更新服务单元。每台机器的 `data/` 都不纳入 Git，更新保留本机配置。0.6 继续兼容 v1 配置，首次保存前生成 `data/config.before-v2.json`，后续保存不覆盖备份。只更新应用无需重装驱动。

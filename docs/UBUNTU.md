# 在另一台 Ubuntu 电脑安装

FanControlLX 的应用代码可以复用；风扇驱动、通道绑定和签名证书按每台机器单独接入。后台安装器默认针对本机 NCT6687 驱动；NVIDIA 或沿用已安装系统驱动的机器使用 `--skip-driver`，避免安装本机主板模块。

## 获取应用并检测硬件

首次发布到 GitHub 后，在目标电脑执行：

```bash
sudo apt update
sudo apt install git build-essential
git clone https://github.com/Skywalker144/FanControlLX.git
cd FanControlLX
# 安装 Rust 1.82+：https://www.rust-lang.org/tools/install
./build.sh
./webui.sh --diagnose
```

构建需要 Rust 1.82+；安装器和常驻后台均无需 Python。部分测试工具需要 Python 3。不需要 Conda、pip、npm 或桌面环境。诊断命令只读取硬件信息。保留完整结果，尤其是 `board`、`kernel`、`chips`、`channels` 和 `gpu_error`，据此选择驱动。普通用户下出现不可写权限提示，不代表驱动不工作。

```bash
./webui.sh
```

在目标电脑打开 <http://127.0.0.1:8090>。如果通过 SSH 远程使用，在浏览器所在电脑建立转发：

```bash
ssh -N -L 8090:127.0.0.1:8090 用户名@目标电脑地址
```

这个首次启动的前台进程用于监控和编辑，尚未安装后台服务，关闭终端会退出。想先看界面可用 `./webui.sh --demo --port 8091`，演示数据与真实配置分开。

## 接入目标电脑的风扇

先根据诊断结果判断：

| 诊断结果 | 下一步 |
| --- | --- |
| 已有受支持驱动和 PWM 通道，仅权限不足 | 停止前台监控，再用 `sudo ./webui.sh --foreground --control` 识别设备 |
| 没有 PWM 通道 | 根据主板与芯片选择并加载对应驱动，重新诊断 |
| 驱动提示尚未验证、仅监控 | 需要验证该驱动的控制和恢复接口，再扩展程序支持 |
| NVIDIA 风扇通道可用 | 可直接确认并控制，无需安装主板模块；参见 [NVIDIA 接入说明](NVIDIA.md) |
| GPU 温度不可读 | 检查目标电脑显卡驱动；优先通过 NVML 读取温度，不可用时退回 `nvidia-smi` |

不要因为本机使用 NCT6687，就给另一块主板安装相同驱动或启用 `msi_fan_brute_force=1`。Linux 自带且受信任的驱动通常不需要自行签名；只有需要第三方模块且启用 Secure Boot 等验证机制时，才需要按目标系统的要求签名并登记证书。驱动必须匹配目标电脑正在运行的内核。

有可用通道后，在设备设置中确认风扇/水泵类型并命名；不确定位置时可用 100% 测试与滑块辨认。再到风扇卡片自行选择温度源和曲线。多个风扇接在同一分线器上，通常只能作为一路控制。对没有已验证测速映射的驱动，需要手动配对 PWM 与 RPM。

不要直接复制第一台电脑的 `data/config.json`。其中包含设备 ID、通道、身份确认及自动控制策略。需要复用曲线时，在目标电脑重新识别并绑定设备后，录入相同的曲线节点与参数。

## 安装后台服务

若目标机器也使用已经验证兼容的 NCT6687 驱动，需要在目标机器针对其当前内核构建模块，准备好签名文件 `.runtime/nct6687d/nct6687.signed.ko`，并完成该机器需要的证书登记。先通过前台控制确认设备，保存配置并结束前台进程，然后执行：

```bash
./install-service.sh --dry-run --enable-confirmed
sudo ./install-service.sh --enable-confirmed
systemctl is-active fancontrollx
systemctl is-enabled fancontrollx
./webui.sh
```

前两条状态检查应分别返回 `active` 和 `enabled`。再在网页确认每路均正在控制、温度与转速正常。具体后台行为见 [BACKGROUND.md](BACKGROUND.md)。默认安装路径要求已签名 NCT6687 模块，即使目标系统没有启用 Secure Boot 也会进行这一检查；`--skip-driver` 跳过该流程。

如果目标电脑使用已有的受支持驱动，或只需要控制 NVIDIA 显卡，保存前台配置并退出后执行 `sudo ./install-service.sh --skip-driver`。该选项不安装或加载主板模块，沿用已经工作的系统驱动；之后在网页逐路启用控制。

签名私钥、内核模块、真实配置和运行数据不包含在 Git 仓库中。不要把第一台电脑的签名私钥当作应用文件搬过去；第三方驱动在目标电脑单独构建并处理信任。

## 更新应用

应用已经安装为后台服务后，在项目目录执行：

```bash
git pull --ff-only
./build.sh
sudo systemctl restart fancontrollx
systemctl status fancontrollx --no-pager
```

配置保存在被 Git 忽略的 `data/` 中，`git pull` 不会覆盖它。0.4 兼容旧版配置，首次保存前会生成 `config.before-v2.json`，原有绑定、名称、曲线、温度源和控制开关保留，无需重新识别设备。更新后刷新网页。修改服务模板或重新构建驱动后，需要再运行安装器；普通代码更新重启服务即可。内核升级后必须重新编译、签名、安装第三方模块，当前没有 DKMS 自动重建。

首次从 Python 后台升级到 0.6，构建后需要运行安装器，将主服务与 NVIDIA 恢复钩子切换为 Rust；沿用系统驱动的机器保留 `--skip-driver` 选项。之后每次代码或网页更新也必须重新构建后再重启。

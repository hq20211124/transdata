# 计划：transdata Rust 版 —— 删 Python 旧版 + 文件夹直传 + 网络代码层修复

日期：2026-07-03（会话：Agnes）
范围：`<项目根目录>`。Python 版（`server/`）确认废弃；`transdata-rs/` 为唯一版本。

---

## 任务 1：删除 Python 版

- 删除整个 `<项目根目录>\server\` 目录（app.py / gui.py / exe / spec / uploads）。
- 前置检查：`server\uploads\` 里若还有历史文件，先列出来问用户是否保留；空则直接删。
- 根目录 `td_err.log / td_out.log / td_dbg_err.log`、`transdata-rs\patch_*.py`、`gui_*.png`
  是开发期调试痕迹，**默认不删**（避免误伤），留待用户确认。
- 更新 README：去掉 Python 章节，改成 Rust 版说明 + 编译命令。

## 任务 2：文件夹直传（Rust 版）

原则：**保留目录结构、分批上传、断线不留残文件、事后清单校验**，不加口令/证书之类额外门槛。

### 客户端 web/index.html
1. Upload 页新增 "Upload folder" 按钮（`<input type=file webkitdirectory multiple>`），
   与现有"选择文件"并存。Safari 不支持 webkitdirectory 时该按钮退化为普通选文件，不报错。
2. 目录文件用 `webkitRelativePath` 作为逻辑路径，`formData.append('file', file, relPath)` 带上；
   服务端按相对路径落盘。普通文件（无 relativePath）照旧落传输目录根。
3. **分批上传**：每批 ≤ 32 个文件且 ≤ 256MB，逐批 POST，逐批显示
   `批次 i/N + 总进度条`。单批失败自动重试（3 次，间隔 2s）；重试前先查清单跳过已完成。
4. **完成后清单校验**：POST `/verify` 提交 `{files:[{path,size}]}`，服务端逐条返回
   ok / missing / size_mismatch。UI 显示"共 N 个文件，全部校验通过"或列出失败清单。
5. 可选"严格校验 SHA-256"：上传前逐文件计算哈希一并进 /verify（10GB 约多几分钟，默认关）。
6. 文件数 > 50,000 时给出警告但不阻断（防浏览器 Files 列表爆内存）。

### 服务端 src/http/server.rs + config
1. `sanitize_filename` → `sanitize_path`：
   - `\` 统一为 `/`；禁止绝对路径（`C:\`、`/`开头、UNC `\\`）、任何 `..` 段、控制字符。
   - 这是**正确性**要求（往子目录写盘），不是安全加固，不加白名单复杂度。
2. 建文件前 `create_dir_all(parent)`。
3. **断线清理**：upload 循环中 read 报错/提前 EOF → 删除本连接已写出的截断文件（当前打开的
   `file: Option<File>` 及本批已完成的文件在 manifest 校验中会以 size_mismatch 暴露）。
4. 批次并发保护：新增全局 `std::sync::Mutex<()>` 串行化上传的"查同名→建文件"临界区，
   防两个浏览器窗口同时传同名文件夹产生 `_1` 竞态（成本一行）。
5. 同名冲突沿用现有文件级 `_1/_2` 后缀规则，不做目录级改名（最小改动；重复传同一文件夹
   效果为各文件加 `_n`，可接受）。
6. `/files` 改**递归**列出（返回相对路径，`name` 字段带子目录前缀），上限 10,000 条，
   超限加 `"truncated":true`；`/download/<enc路径>`、`/delete/<enc路径>` 接受编码后的相对路径，
   先过 sanitize_path 再拼目录。
7. 接收端下载页自动显示子目录文件；仍为逐文件下载（**不做**服务端一键 zip，
   避免引入 zip 依赖；真实场景 1GB 压缩包本来就是单文件，够用）。

### 测试
- 单测（沿用现有风格）：
  - 两级目录 multipart 上传 → 落盘位置正确、字节一致；
  - `..`、`C:\x`、`\\server\share`、绝对路径 → 拒绝且 400；
  - 模拟断线（客户端提前 close）→ 传输目录无截断残留文件；
  - 两个同名文件夹先后上传 → 第二个文件带 `_1`；
  - /files 递归 + /download 子路径 200。
- 手测：5000 个小文件文件夹 + 1 个大文件，对照客户端清单全 ok。
- 重新 release 编译 exe，替换产物并记录路径。

## 任务 3：网络传输 —— 只修代码层

先审计结论（代码层没有 2MB/s 级别的结构性瓶颈，但有三处可修）：
1. 连接建立后 `set_nodelay(true)`（当前没设，Nagle+延迟ACK 在弱 RTT 下会显著压速）。
2. download 循环里阻塞式 `std::fs::read` 在 async 任务中执行 → 改 `tokio::fs` /
   `spawn_blocking`，避免卡住 worker 线程。
3. 去掉每 256KB 一次的冗余 `sock.flush()`（TCP 下无实际收益，纯系统调用开销）。

README 加 3 行"传输速度低"的排查说明（网卡链路速率/杀软/Wi-Fi），
**环境侧问题不在本应用范围内解决**，不写代码。

---

## 反向质疑（自我反驳）与裁决

| 质疑 | 裁决 |
|---|---|
| 删 Python 会不会误删在用的 exe？README 里 dist exe 是 Python 版产物 | 用户已确认只用 Rust 版；删前列 `server\uploads` 内容过一眼 |
| "不丢包不乱"只验 size，网络层数据损坏怎么办？ | TCP 本身保证字节序不丢不乱（传输层）；size 清单是防"截断/落盘阶段"问题的防线；SHA-256 做可选项覆盖极端场景。成本收益合理 |
| 10 万文件的文件夹浏览器会不会炸？ | 5 万预警 + 不阻断；真实场景（1GB 压缩包）远达不到，属防御性提示 |
| 文件级 `_1` 后缀在文件夹场景是否丑陋？ | 丑但不乱、不覆盖、可恢复；目录级改名引入"改名后后续批次路径失配"复杂度，明确不做 |
| 递归 /files 对超大目录性能？ | 10,000 条上限 + truncated 标记；read_dir 遍历 10k 级目录 <100ms，可接受 |
| spawn_blocking 改 download 有没有回归风险？ | 行为等价（EPIPE 同样终止循环）；风险低但非零，随测试覆盖 |
| nodelay 对上传方向（浏览器→服务器）无效，是否白做？ | 不是白做：影响下载方向（服务器→接收端），2MB/s 卡顿更可能出现在下载；上传方向由浏览器管理 Nagle，无代码可改 |
| GUI wgpu 纹理 panic（td_err.log）要不要顺手修？ | 开发期 9900px 截图工具触发，真实窗口 <2160px 不触发；用户要求保持简单，不修，留日志即可 |

## 待确认（批准后开工）

1. 删 `server/`（uploads 内容先看一眼）——OK？
2. 文件夹直传：默认 size 清单校验 + 可选 SHA-256 严格模式——OK？
3. 接收端逐文件下载、不做服务端 zip——OK？
4. 网络只做 nodelay/tokio::fs/去flush 三处代码修复，环境不碰——OK？

---

## 执行记录（2026-07-03，用户批准后实施）

用户批复：① uploads 合并进 Rust 版 uploads 目录后删 Python 版；② 删 Python 版不单独留痕；③ 文件夹方案照计划；④ 新增：**做服务端 zip 一键下载**（参考 LocalSend 类实践，浏览器场景下流式 zip 是最省事的接收端方案）；⑤ 网络只改代码。

已完成：
- `server/` 整目录删除；`<历史 APK> / <历史 APK> / <GUI 配置文件>` 已并入 `transdata-rs/target/release/uploads/`
- 服务端 `src/http/server.rs`：
  - `sanitize_rel` 路径白名单（拒 `..`/绝对路径/盘符/控制字符），`sanitize` 失败回退 safe_leaf
  - 上传支持子目录（`create_dir_all`），`UPLOAD_LOCK` 串行同名临界区，断线自动删截断文件
  - `/files` 递归（上限 1万 + truncated 标志，跳过点文件）；`/delete` 支持整棵目录树
  - 新增 `POST /verify`（path+size，可选 sha256，上限 2 万条）；新增 `GET /zip/<dir>`
    （临时文件流式压缩→socket 流式发送→删临时文件，内存恒定；点文件不入库）
  - 下载改 `tokio::fs` 异步读、去每块 flush；所有连接 `set_nodelay(true)`
- 依赖新增：serde_json / zip(deflate) / sha2
- 网页 `web/index.html`：文件夹选择（webkitdirectory）、分批（≤32 文件且≤256MB/批，
  单批重试 3 次，传完自动 /verify 跳过已完好文件）、可选 SHA-256 严格模式（http 非安全上下文自动退化为大小校验）、下载页 📁 行 + Download as zip
- 测试：11 个全绿（含新增：子目录结构保留 / 穿越回退 leaf / 断线清理 / 递归 files / verify 三态 / 删目录树 / zip 冒烟）
- `cargo build --release` 通过；真实 exe 冒烟：/files /verify /zip(内容可用 Python zipfile 解包验证) /delete 树 全部正常，临时 zip 已清理
- README 重写为 Rust 版说明 + 速度排查 4 条；Python 版删动不另行记录

## 追加（同日）：多选手势常驻提示（用户确认不知道长按=多选，要求常驻）

- 「上传文件」卡片下新增一行小灰字：「提示：选择器里长按第一个文件可多选；也可以反复点卡片累加」
- 删除之前“仅单选时弹提示”的逻辑（被常驻提示取代）；已编译验证

## 追加（同日）：安卓多选手势提示

- 用户反馈「只能选单个」：非退化，是安卓 DocumentsUI 点按=立即确认单选、长按=进入多选；
  微信内置浏览器也会把 multiple 降级单选。当「上传文件」卡片只返回 1 个文件时，
  显示一条一次性提示（长按多选 / 再点卡片累加），多选成功则不显示；已编译验证

## 追加（同日）：健壮性与文案 v3

- 上传前预检 /info（3s 超时）：服务端不可达时**立即**报「无法连接中转服务器…」，不再干等 3×2s 重试
- 每次批次尝试都有即时状态（第 N 次失败 2s 后重试…）；失败时保留待传列表，再点「确认」自动续传
- 底部按钮「上传」→「确认」（避免与「上传文件」卡片语义冲突）；成功后按钮禁用为「✅ 完成」
- 说明：iOS 浏览器不支持 multiple 多选（选一个即弹回），累加模式（反复点选 + 去重 + ×删除）即为其替代方案；Android 可正常多选

## 追加（同日）：细节微调 v2（用户对照手机实机反馈）

- 地址行加前缀「地址：…　目录：…」；卡片文案精简为「上传文件 / 上传文件夹」
- 待传文件列表每行加 × 删除（removeFile，可逐个取消）
- 便签标题改「自动同步」；textarea min-height 48→120px（填补手机底部空白）
- 重建 release 并同步副本；curl 验证 6 处标记全部生效

## 追加（同日）：手机端 UI 修正（用户手机实测反馈）

- 手机浏览器不支持 webkitdirectory（Web 限制）→ 点「选整个文件夹」退化为普通多选时
  给明确提示 + 建议先压缩 zip；电脑端不受影响（相对路径仍保留）
- 漏藏的 `<input id=folderInput>` 原生控件（“No file chosen”）→ 与 #fileInput 一起 display:none
- 卡片文案改为「选多个文件（散文件，不带目录）」「选整个文件夹（保留目录结构，电脑浏览器推荐）」
- 删除 SHA-256 严格校验勾选项（降低使用门槛；/verify 的 hash 能力保留在接口层不用）
- 重建 release，`uploads/transdata.exe` 副本同步；curl 验证新页生效

## 追加（同日）：下载列表定案：全量铺开 + 页面自然滚动（用户拍板）

- 放弃分页/嵌套滚动方案：列表一次渲染全部（上限 200 行保护 DOM），整页自然滑动；
  彻底移除 overflow 嵌套容器 → X-browser 紫色渲染 bug 不再触发
- 顶部「共 N 个文件」总数行 + 单顶层文件夹时的「全部打包下载 zip」保留
- 中间过程：先加白底防御（未根治）→ 分页+上一页 → 最终选回滑动全展开，定稿

## 追加（同日）：定稿收官 —— 回归朴素分页（用户最终拍板）

- 下载列表定案：每页 8 行 + 紧凑行（行 padding 4→2px、按钮 4→3px）+「上一页/N页/下一页」按钮；
  数据一次拉回本地，翻页纯本地切；无嵌套滚动框，X 浏览器无渲染坑
- 搜索框方案（曾加过）已移除

## 定稿清理（完成）

- 删构建垃圾：target/debug（1.5G）、release 下 build/deps/incremental/.fingerprint/PDB 等 → target 仅剩 37M
  （transdata.exe + uploads 用户数据）；src/gui 空目录、根目录测试日志一并清掉
- 最终验证：cargo test 11/11；exe 启动 /files 正常
- 目录定稿：README + PLAN + transdata-rs 源码 + target/release/transdata.exe（交付物）


- 根因：win32_set_clipboard 把 UTF-16 码元存成了 **u32（4 字节）槽位**；
  CF_UNICODETEXT 要求 u16（2 字节）序列，Windows 按 2 字节读时第二字位置恒为 00 00
  即 NUL → 字符串提前终止，只剩第一字。改为 Vec<u16> + NUL、byte_len = len*2
- 已编译同步（含 test 11/11）；需用户重启 exe 验证

## 追加（同日）：下载列表可发现性修正

- 列表回到 200px 内部滚动框（手机不超一屏），但：
  - 滚动框外常驻「共 N 个文件」总数行；只有一个顶层文件夹时同行直给「全部打包下载 zip」
  - 滚动框内尾部加「↓ 共 N 项，继续下滑查看」提示（>5 项时）
  - 文件数 >500 时只渲染前 500 行 + 红字提示（防手机 DOM 卡死）
- 顺手把 deleteItem 的确认/报错文案中文化；node 校验整段 JS 语法通过

## 追加（同日）：定稿清理（同日）

- 删开发期垃圾：根目录 `gui_shot.png / td_*.log`；`transdata-rs/` 下 4 个 patch/check 脚本 + 36 张 gui_*.png 截图
- 保留：源码 + icon 源文件（icon_512/32、icon.ico/icon.rc）+ build.rs + web/index.html + target（release 产物可用，debug 1.4G 供开发）
- 最终验证：cargo test 11/11；新 exe 启动 /files 正常；目录干净，**定稿**

## 追加（同日）：页面中文化 + 紧凑布局（手机单屏不滚动）

- 页面全改中文；删顶部 File Transfer / 副标题
- Server URL 卡片合并为一行「🌐 地址 📁 目录」（超长省略号，title 可看全文），删 save-dir 大卡片
- 卡片顺序：地址 → 上传/下载 → 便签（便签降为附件功能放最后）
- 提示简化：「可选择多个文件」「可选整个文件夹（保留目录结构）」
- 纵向收窄：卡片 padding/margin、字号、textarea、按钮全部压缩，估算手机视口内总高 ~400px，不滚动；
  文件列表保留 130px 内部滚动，不影响主页面
- 按钮文案中文化（上传/下载/删除/打包下载 zip）；重建 release + 同步副本 + curl 验证


- 设计：扁平靛紫渐变（#667eea→#764ba2，与界面同色）圆角方形 + 白色「笔记本 ↔ 手机 + 双向箭头」；
  源文件 `icon_512.png`，PIL 程序化绘制（未用在线图片生成，无需账号）
- `icon.ico`（16/24/32/48/64/128/256 多尺寸）+ `icon.rc`（`1 ICON "icon.ico"`）+ `build.rs`
  （embed-resource 走 RC.exe；注意直接喂 .ico 会被 RC 当脚本忽略，必须经 .rc 包一层——踩过）
- 窗口/任务栏图标：`gui.rs::window_icon()` 编译期内嵌 `icon_32.png`（image 解包→egui IconData）
- 验证：从新 exe 资源提取图标确认为本设计；exe 启动 5s 无崩溃；
  `uploads/transdata.exe` 旧副本已同步为新版


# 计划：transdata Rust 版 —— 删 Python 旧版 + 文件夹直传 + 网络代码层修复

日期：2026-07-03（会话：Agnes）
范围：`<项目根目录>`（即本仓库）。Python 版（`server/`）确认废弃；`transdata-rs/` 为唯一版本。

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
- `server/` 整目录删除；其中遗留的若干历史 APK 与配置文件已并入 `transdata-rs/target/release/uploads/`
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

## 追加：电脑端「快捷文本」粘贴长文本后按钮被挤占（已修复）

现象：电脑端窗口（egui GUI）的「快捷文本」输入框里**一次粘贴长文本**，
下面的「复制 / 发送」按钮就看不见了（被挤出窗口）。

根因（egui 0.28 的多行 TextEdit，两个坑）：
1. **高度无上限**：TextEdit 的高度 = 文本排版后的高度（`galley.size().y`）。
   4400 字中文排版后约 1050px 高，而主窗口固定 860×560 不可缩放 →
   按钮被顶到 y≈1097，直接跑到窗口外面。
2. **默认只在空格处断行**：粘贴一整段没有空格的长文本（URL / base64 / 中文）
   → 排版宽度暴涨 → 输入框把左右按钮顶出屏幕。

修复（`gui.rs::note_text_edit()`）：
- 外层套 `ScrollArea::vertical().max_height(120)`：输入框占位高度恒定 ≤120px，
  超出的内容在框内滚动（粘贴后 egui 的 `scroll_to_rect` 自动滚到光标处），
  按钮位置永远固定在输入框正下方。
- 内层自定义 `layouter`：`job.wrap.break_anywhere = true`，任意字符都能断行。
- 输入框行数 4 → 6（配合限高 120px）。

验证：新增无头布局回归测试 `gui::tests::long_text_must_not_push_buttons_out_of_window`
（egui `Context::run` 模拟 860×560 窗口）：
  旧写法 输入框占位 1055px / 按钮 y=1097（窗外）→ 新写法 占位 121px / 按钮 y=163（窗内）。
另用无头渲染出图肉眼确认：输入框被裁到 120px，按钮在框正下方可见。
`cargo test --release` 12/12 通过；release 重建 + `uploads/transdata.exe` 副本已同步。

网页端（手机/电脑浏览器）无此问题：textarea 高度固定、按钮在输入框上方，
已用 Playwright 在 1280×900 下实测粘贴长文本后布局不变。

## 追加：开源仓库信息脱敏（仓库保持 public）

仓库按用户要求继续开源，但把文档里能指向本人本地环境的痕迹去掉：

- 绝对本地路径（盘符 + 项目目录名）→ 统一写成 `<项目根目录>`
- 遗留的其他项目产物名（历史 APK 名、某个 GUI 配置文件名）→ 泛化为「若干历史 APK 与配置文件」
- 保留：路径穿越示例（`C:\x`、`\\server\share`）、本项目自身的开发期文件名
  （`td_*.log`、`gui_*.png`、`icon_*.png`）——这些是程序行为说明，不指向本人

同时核查：仓库内**无任何**密钥/口令/token/私钥；`transdata-rs/target/` 已被 .gitignore 忽略，
真实传输目录（PDF、截图、APK、日志、exe 副本）从未入库，远程文件树只有源码/图标/文档。

附记：README.md 一度显示为乱码，实为本地终端 GBK 代码页的显示问题，
文件本身是合法 UTF-8（无 BOM、无 U+FFFD、无 mojibake），无需改动。

## 追加：性能 + UI 黏性 + 安全，一次做完（实测前后对比）

先做了一轮压测定位瓶颈（loopback + NVMe），再逐项改，改完用同一套脚本复测。

### 改前实测（基线）

| 指标 | 改前 |
|---|---|
| 上传 256MB | 17~19 MB/s（服务端 CPU 5.9s/16s，磁盘 342~575MB/s 不是瓶颈） |
| 下载 256MB | 96 MB/s |
| HTTP 头（+1KB） | 14~31 ms（**逐字节读头**，每字节一次 syscall + reactor 唤醒） |
| GET /files（2000 文件） | 557 ms / 110 KB（每次全量重扫目录树） |
| 首页体积 | 30.4 KB，未压缩，`no-store` → 每次全量重下 |
| 跨站读 /files | **200 OK**（`Access-Control-Allow-Origin: *`，任意网站可读/删本机文件） |

### 改后实测（同脚本同机器）

| 指标 | 改后 | 变化 |
|---|---|---|
| 上传 256MB | **338 MB/s** | **18x** |
| 下载 256MB | **668 MB/s** | 7x |
| HTTP 头 +1KB | **1.35 ms** | 10~23x |
| GET /files（2000 文件） | 首次 146 ms，之后 **4~8 ms**（2s 缓存，上传/删除即失效） | ~70x |
| 首页体积 | 37.1 KB 原始 / **14.4 KB gzip** + ETag（重载 304） | 传输量减半以上 |
| 跨站读 /files | **blocked**（无 ACAO + 令牌校验 → 401） | 安全修复 |
| GUI 空闲 CPU | 0.2% 单核 | 保持 |

### 具体改动

**安全（P0）**
- 去掉所有响应里的 `Access-Control-Allow-Origin: *`（页面同源，本来就不需要 CORS）
- 每次启动生成 32 位随机会话令牌（OS RNG）：页面请求带 `X-TD-Token` 头，
  下载/打包链接与 WebSocket 用 `?t=`，页面响应下发 `HttpOnly; SameSite=Strict` Cookie
- 服务端除页面本身外一律校验令牌，缺失 → 401（新增单测 `session_token_is_required`）
- 效果：手机/电脑浏览器里打开的任意网页都无法再列出、下载、删除本机传输目录

**服务端性能**
- 上传：分隔符扫描 `w.windows(n).position()` → `memchr::memmem`（SIMD）
  ——这是上传慢的头号元凶（256MB 烧掉约 5s CPU）
- 上传：`std::fs::File` 阻塞写 → `tokio::fs::File` + 1MB `BufWriter`（不占 worker）
- 上传：每块 `split_off` 重新分配 → `drain` 原地搬移
- HTTP 头：逐字节读 → 4KB 块读 + 找 `\r\n\r\n`；读超的请求体字节交给正文读取器
  （新增 `read_body` / `read_body_exact`，避免"头和体粘在一个 TCP 段"时丢数据）
- `/files`：加 2 秒缓存（上传/删除立即失效）
- 首页：预压缩 gzip + ETag + `must-revalidate`（304）

**网页端 UI**
- 上传进度条：改用 XHR（fetch 没有上传进度），显示 已传/总量、百分比、实时速度、剩余秒数
- 上传可「取消」：中断后保留已传部分，再点「确认」自动续传
- 下载列表加「刷新」入口（原来只能切页签触发）
- 便签标题旁显示字数 + 最近同步时间
- 令牌注入页面，所有 fetch/链接/WS 自动带上

**桌面端 UI**
- 便签文本不再每帧克隆两遍（打字时 60fps 拷贝长文本）：改用常驻 `text_buf`，
  只在真的敲了字时才留副本
- 窗口标题栏新增「● 传输中 (n)」：后台线程每 2s 轮询 `/info` 拿活动传输数
  （传输发生在浏览器里，之前桌面端完全看不出在传东西）

### 验证
- `cargo test --release` 13/13 通过（新增令牌单测；原有 12 个全绿）
- 端到端脚本复核：WS 握手 101、便签收发一致、WS 推送收到、无令牌 401、有令牌 200
- Playwright 走真实浏览器：令牌注入、进度条（含速度/百分比）、取消续传、
  刷新入口、便签字数、跨站读取 blocked、无控制台报错
- 上传完整性：256MB 文件 MD5 源/收一致
- 手工渲染截图确认进度条界面




## 追加（同日）：UI 重做 + 安全重构（"设计师说 UI 太丑 + 不够安全"）

> 本节原文在早期一次编码事故里变成了乱码（UTF-8 被按 GBK 解读后又被替换成
> U+FFFD，原始字节已不可还原）。2026-10-28 按上下文与代码现状重写，事实以代码为准。

### 一、UI 重做（web/ 三件套 index.html + app.css + app.js）

- 视觉：浅色/深色双主题（`prefers-color-scheme` 自动切换）+ CSS 变量；
  圆角卡片 + 柔和阴影；系统字体栈 12/13/14px 字号递进；可点击元素用主色；
  emoji 图标；**去掉所有内联 onclick**
- 结构：顶部双 Tab（上传 | 下载），卡片等高；手机端底部吸底输入框；
  顶部 = 品牌 + 连接状态胶囊 + 复制地址
- 上传：拖拽区；文件列表带图标/大小/进度/移除；显示已传/总量、百分比、
  实时速度、剩余时间、取消
- 下载：文件行分组 + 下载/删除按钮；超过 200 条截断提示
- 便签：字数统计 + 最近同步时间；复制 / 发送
- 安全写法：全部 `addEventListener` 绑定（无内联 handler），用户数据（文件名等）
  一律 `textContent` 写入、不拼 `innerHTML` —— 这样才能配严格 CSP

### 二、安全重构（换模型，令牌不再下发给任何人）

- 问题：`GET /` 把 token 直接注入 HTML 发给**任何**请求者 —— 同一局域网任何设备
  `curl` 一下即拥有全权限（列目录/下载/删除/上传）。token 只挡得住「跨站网页」，
  挡不住「同网段设备」，不符合「配对才能用」的初衷
- 新模型：设备级配对，一次 `HttpOnly; SameSite=Strict` 会话 Cookie；页面里不放任何凭据
- 配对码只出现在桌面窗口；二维码用 URL 片段 `#k=`（片段不会发给服务器）+ 8 位数字码
- 会话 12 小时无操作失效；每设备 5 分钟 10 次限速；30 次失败锁定
  （v1.2.0 把「永久锁定」改成了 5 分钟冷却 + 一键换码）
- 桌面端自己用 `X-TD-Secret` 头（同机、不经网络边界）
- 凭据比较改常量时间 `ct_eq`；删掉 URL 上的 `?t=`（不进浏览器历史/日志）
- 写操作校验 `Origin`，跨站 403；任何响应都不带 CORS 头
- 路径加固：`..` / 绝对路径 / 盘符 / UNC / Windows 保留名（CON、NUL、COM1..）/
  非法字符 `<>:"|?*` / 尾随空格 全部拦；符号链接与 junction 不能跳出传输目录
  （canonicalize + 前缀校验）
- 磁盘：按剩余空间动态算（至少留 1GB），超出 507；并发上传上限 8；便签 4MB
- 响应头：严格 CSP（`script-src 'self'` / `style-src 'self'`，无内联脚本/样式）
  + nosniff + X-Frame-Options + Referrer-Policy + COOP/CORP + Permissions-Policy
- zip：边压边传，不落临时文件；跳过点目录/点文件

### 三、验证

- `cargo test` 22/22
- Playwright + 真实 Chrome 渲染检查：浅色 / 深色 / iPhone 13 / 桌面页，
  console 无错误、无 CSP 违规
- 已知边界写进 README：默认不加密（另开可选 HTTPS 开关）

## 追加（同日）：HTTPS 开关（默认关闭）+ 自签证书 + 指纹校验

### 一、HTTPS（可选，默认关）

- 窗口「访问地址」卡片下面的开关，勾上立刻生效（端口不变、协议切换、
  地址和二维码自动跟着变）；服务端用 watch 通道感知变化后重新 bind
- 证书自签（rcgen 0.13 + rustls 0.23/ring + tokio-rustls 0.26）：
  - SAN 覆盖本机所有 IPv4 + localhost
  - 证书与私钥**持久化**在程序目录（不可写则落 `%LOCALAPPDATA%\TransData`），
    重启复用，只有本机 IP 变了才重签 —— 手机只提示一次
  - 窗口显示 SHA-256 证书指纹，和手机证书详情里的指纹对得上
- 同一端口探测首字节（`0x16` = TLS）：不是 TLS 就给一张「请改用 https」的页面，
  而不是一堆看不懂的网络错误
- HTTPS 模式下同源校验按 https 判断（写死 http 会误伤自己 —— 实测踩到过）
- 响应统一 `close_notify`，TLS 客户端不再报 unexpected EOF
- 配对 Cookie 在 HTTPS 模式下带 `Secure`
- 所有 handler 泛型化到 `trait Conn`（AsyncRead + AsyncWrite），HTTP / HTTPS 共用一套代码

### 二、桌面端精简

- 删掉「复制」旁边的手动刷新按钮（没有别的设备时刷新毫无意义）
- 不再内置 HTTP 客户端（原来为了「远端标签」定时回窗口）：改成一条 WebSocket
  推送 + 每 2 秒轮询 `/info`；note 版本号变化才触发重绘
- 窗口 880×720，给 HTTPS 开关 + 指纹留位置

### 三、验证

- `cargo test` 24/24（HTTPS 模式下跑通全部 API；http 打 https 端口的提示页；
  同源校验按协议切换；WS 鉴权）
- 真实 Chrome + Playwright 跑 HTTPS 全流程：配对 → 上传 → 校验通过 →
  下载 → 打包
- 窗口用 egui 无头渲染出 PNG 人工核对

## 追加（2026-10-20）：界面大改版（用户按手机实机反馈逐条返工）

**网页端**
- 结构重排：顶部状态条 + 上传/下载 Tab 切换 + 底部**吸底发送框**（快捷文本）
  ——发消息不用再滑到页面最底；手机端整屏 app 布局，页面高度恒等于一屏
- 文件列表默认只显示 5 条（上传/下载都是），展开后在**列表内部滚动**，
  页面永远不会被几十个文件撑长；不再显示「共 N 项 / 还有 N 项」这类
  会把「文件数」和「项目数」混在一起的统计
- 下载列表改成**目录树**：目录带 ▸ 可展开，**只有目录才有「打包」**，
  文件行只有「下载」「删除」；目录行有独立底色和「文件夹 · N 个文件 · 大小」标注
  （此前子目录里的文件在界面上根本看不到，只能整包下载）
- 新增 GET /zip-all：把整个传输目录打成一个包（此前只有「根目录恰好一个
  文件夹」时才能全量下载）
- 修 zip 的两个 bug：根目录未 canonicalize 导致 strip_prefix 失败、
  **压缩包里所有条目名为空**；打包时跳过 .trash 等点目录
- 修**静态资源缓存**：/app.css、/app.js 原来发 max-age=3600，
  手机会**缓存一小时且不重新校验** → 重新编译后手机仍在用旧样式（列表不滚动、
  按钮失效）。改为 
o-cache + ETag（304 依旧很便宜）

**桌面端**
- 去掉系统标题栏（with_decorations(false)），自绘标题栏：图标 + 名称 +
  运行状态 + 最小化/最大化/关闭；标题栏可拖动窗口、双击最大化
- 窗口四边圆角（SetWindowRgn 裁圆角矩形；最大化时自动取消圆角）
  ——踩了两个坑：EnumWindows 回调里调 GetWindowTextW 会给自己线程的窗口
  发 WM_GETTEXT → winit 事件代理死锁（窗口根本起不来）；改成只用不发消息的
  GetWindowRect 比面积认主窗口
- 二维码卡片瘦身：只留「扫码连接  或用配对码」+ 二维码 + 配对码（可点击复制），
  两段说明文字删除
- 快捷文本输入框：圆角外框 + 自绘边框，聚焦时边框是主色细线
  （egui 默认无圆角、聚焦时一圈深灰描边）；ScrollArea 移进外框内，
  输入框和卡片左右边缘严格对齐；去掉了输入框下方那块空白
- 窗口 880×720 → 880×570；访问地址卡片里的 HTTPS 说明 + 证书指纹压成一行

**验证**
- cargo test 24/24；桌面端用 PrintWindow 抓真窗口核对（圆角、聚焦边框、
  最大化/还原、输入框可打字）；网页端用 Playwright 对真实服务器跑
  390×844 / 1280×900，核对配对 → 发送 → 展开目录树 → 嵌套文件单独下载 →
  嵌套目录单独打包（zip 条目名逐条核对）

---

## v1.1.0：设置 / 深浅色 / 在线更新 / 窗口可缩放

**需求**（用户提的 4 条）
1. 标题栏加设置按钮，里面先放一项「关于」——显示最新版本号
2. 支持在线更新
3. 设置按钮放在最小化旁边，设置旁边再放一个月亮/太阳（深浅色切换）
4. 窗口可以拖边缘改大小

**做了什么**
- 新增 `src/settings.rs`：主题 + 更新源写在 exe 同目录的 `settings.json`，
  启动时读、改主题时写（读不出来就用默认值，坏文件不会挡住启动）
- 新增 `src/update.rs`：查版本 → 下载 exe → 校验 sha256 → 换掉正在运行的自己
  → 重启生效。换 exe 用「先改名再换入」的办法（Windows 上运行中的 exe 不能删，
  但能改名），第二步失败会把旧文件换回去，不会把程序弄丢
  - 版本源支持两种：GitHub Release（默认，取 tag_name + .exe 附件）和自定义 JSON
    清单（自己的服务器也行，settings.json 里改 update_url）
  - 服务器一个版本都没发过时不算错误，按「没有新版本」处理
- 桌面端配色从常量改成 `pal::now()`，浅色/深色两套；标题栏多出
  **月亮/太阳**（点一下立刻切，记住选择）和**齿轮**（打开设置面板）
- 设置面板目前只有「关于」：应用名、当前版本、最新版本 + 检查更新 / 立即更新 /
  立即重启；程序启动时先自动查一次版本，面板里直接能看到「最新版本」
- 窗口可缩放：系统标题栏关掉后没有系统边框可拖，所以自己实现——光标靠近
  边缘（6 像素）换成缩放光标，按下后抓住鼠标，按位移算新矩形（SetWindowPos）；
  最小 640×470；顶边留给标题栏拖动，只有四个角能改高度
- 标题栏顺带显示版本号 v1.1.0

**坑**
- 合成鼠标事件在这台机器上被系统拦了（前台 IDE 是提权的进程，UIPI 不让低权限
  进程往里注入输入）→ 交互一律用**无头渲染 + 模拟点击**验证：
  造 RawInput（PointerMoved + PointerButton）在 egui 里跑一帧，
  断言主题真的切了、设置面板真的开了
- egui 的 Window 有淡入动画，只画一帧的话设置面板还没出现（看起来像没渲染）→
  渲染测试改成连画 20 帧、时间往后推
- 调色板从 const 改成结构体后，字段名大小写要统一（Rust 会警告 non_snake_case）

**验证**
- cargo test 31/31（新增：标题栏按钮能点、设置面板有版本号、拖边缩放与最小尺寸、
  在线更新全链路——起本地假服务器发清单+假 exe，查版本→下载→校验→替换 exe）
- 真窗口 PrintWindow 截图核对：标题栏的月亮/太阳/齿轮、版本号
- 无头渲染出深色主题和设置面板（有新版本时）的截图核对
- 真实 app 启动后日志：update check: UpToDate("1.1.0")（对着真的 GitHub 仓库查）

### 修：窗口拖矮了，底部「复制 / 发送」按钮不见了

**症状**：从右下角往里拖，窗口一变矮，底部快捷文本卡的按钮就被裁到窗外。

**原因**：快捷文本卡的高度是「用剩下的空间」算的（`input_h = 剩余高度 - 94`），
而主区那一行先按整屏高度占位，等于把剩余空间吃光了。窗口一矮，剩下的空间不够
放整张卡，卡就顺着布局流跑到窗口外面去了 —— 按钮跟着一起没了。

**改法**：不再靠布局流推，**先把两块区域的矩形算出来**：
- 快捷文本卡：高度 = 面板高度的 34%（最少 186 = 94 + 输入框 80 + 外边距 12），
  矩形直接贴着面板底部 → **按钮永远在窗口里**
- 主区：拿剩下的高度
- 窗口太矮时按顺序让位：主区 < 300 先藏「全部IP」列表和 HTTPS 长说明，
  < 250 再藏「传输目录」下面那行提示
- 二维码跟着主区高度缩（72~118px），不会被裁

**验证**：新增无头渲染测试 `render_small_sizes`，把窗口渲染成 880×570 / 500 /
470（允许的最小高度）/ 440 / 700×470 五种尺寸，靠找主色像素定位「发送」按钮，
逐个确认按钮都在窗口内（改之前 500 以下就「按钮被裁掉了」）。

### 顺带：打开就是最小尺寸 + 两张卡片别离太远

- 窗口初始尺寸改成最小尺寸（640×470）：省地方，嫌小可以随时拉大
- 「访问地址」和「传输目录」两张卡片之间的间距：原来卡片外边距 12 + 额外
  add_space(12) + 布局间距 8 = 32px，太散 → 去掉多余的 add_space，
  外边距降到 6 → 现在 14px

---

## 修复计划：v1.2.0（2026-10-28，通读源码 review 后）

来源：一轮对抗式通读（server.rs 3674 / gui.rs 3148 / app.js 1014 / config / tls / update / net）。
现状基线：`cargo test` 38 passed + 4 ignored（ignore 的全是 GUI 出图）；工作区另有 977 行未提交改动。
下面每一条都标了「改哪里 / 怎么改 / 怎么证明改对了 / 风险」，按批次独立提交，随时可停。

### 批次 0：先立基线（30 分钟）

| 项 | 做法 |
|---|---|
| 0.1 | 把当前 977 行改动（重命名/活动日志/设备列表/临时文件原子发布）作为基线提交，否则后续 diff 分不清谁改的 |
| 0.2 | `.gitignore` 加 `.workbuddy-ai/`（现在是 untracked 的裸目录） |
| 0.3 | 版本号 1.1.0 → **1.2.0**（改动含行为变更，不是补丁号） |

### 批次 A：P0 确定性 bug（半天，全部是低风险局部改动）

**A1 上传列表「×」失效/删错行** — `web/app.js:419-441`
- 改法：闭包里不再用循环变量 `idx`，改成按对象引用删（`removeSelected(it)` → `selected.indexOf(it)`），并抽成独立函数便于测试。
- 验证：Playwright 走真实页面，选 3 个文件点第 2 行 ×，断言剩 2 个且剩下的是第 1、3 个；折叠态（选 12 个只显示 8 行）再点一次 ×，断言删的是点中的那一行。
- 风险：无（纯前端局部）。

**A2 HTTPS 下 `/info` 仍给 http 地址** — `server.rs:659`
- 改法：按 `cfg.tls_on()` 选 scheme；GUI 侧 `base_url()` 已正确，不动。
- 验证：扩展现有 `https_mode_serves_the_api`，断言 `/info` 的 `url` 以 `https://` 开头；http 模式断言 `http://`。
- 风险：无。

**A3 路径双重 URL 解码** — `server.rs:303` + `749/755/766`
- 改法：`parse_request` 只解码一次并保留 `raw_path` 给前缀匹配；`/download/`、`/delete/`、`/zip/` 的 handler 直接切片，**不再二次 `url_decode`**。
- 影响面：所有带路径参数的路由（download / delete / zip / zip-all 不受影响）。客户端 `encPath()` 是逐段 `encodeURIComponent`，一次解码即还原，兼容。
- 验证：新增单测 `percent_in_filename_roundtrips`：上传名为 `50%41.txt` 的文件 → `/files` 列出 → `/download` 200 且字节一致 → `DELETE` 200；再加 `a%2Fb.txt` 边界（解码成子目录路径，应 404 而不是穿越）。
- 风险：中低。这是本批唯一触碰公共解析路径的改动，单独一个 commit，改完先跑全量测试再动别的。

**A4 `/verify` 超 2 万文件集体误报 missing** — `server.rs:39 / 1594 / 1622`
- 根因：`VERIFY_CAP` 同时限制了清单条数和磁盘清单，DFS 走满 2 万就停。
- 改法：删掉「全量扫目录 + HashMap」的做法，改成**按清单逐条 stat**（path 已经过 `safe_rel` + 解析，直接 `metadata` 即可）。`VERIFY_CAP` 保留为「单请求清单条数上限」，与客户端 15000 一批对齐。
- 收益：顺带把大目录下 O(全目录) 的扫描降到 O(清单条数)，2 万条 stat 比遍历 2 万文件更便宜。
- 验证：脚本造 25000 个小文件 → 一次 `/verify` 提交 15000 条 → 断言 `all_ok:true`（旧代码会报 missing）；现有 `verify_reports_status` 三态用例保持绿。
- 风险：中低。注意 symlink 语义：原实现跳过 symlink，新实现需对 `metadata` 前先 `symlink_metadata` 判断并跳过。

**A5 列表截断静默** — `server.rs:37` + `app.js:669/690/274/777`
- 改法：`dlTruncated` 接上 UI，列表尾部加一行「仅显示前 10000 项，其余请用「全部打包下载」」；`RENDER_CAP=200` 触发时同样加「仅渲染前 200 行，共 N 行」。新增 `.listmore` 样式。
- 验证：Playwright 拦截 `/files` 返回 `truncated:true` 与 300 条数据，断言两条提示都出现。

### 批次 B：P1 安全与健壮性（一天）

**B1 WS 便签绕过 4MB 上限** — `server.rs:976`
- 改法：`WsMsg::Text(t)` 里判 `t.len() > NOTE_MAX_BYTES` → 丢弃并回一条错误帧（不广播）；同时给 `accept_async` 传 `WebSocketConfig`，把 `max_message_size` 设成 4MB+ 余量。
- 验证：新增单测：连 WS 发 5MB 文本 → 连接被关闭 / `read_note` 内容不变；发 1MB → 正常同步。

**B2 配对锁可被 DoS** — `config.rs:18` + `server.rs:797/843`
- 现状：30 次失败全局永久锁，只增不减，无解锁入口 → 局域网任意人 30 次错码即可锁死配对。
- 改法（建议）：`pair_locked: AtomicBool` → `pair_locked_until: Mutex<Option<Instant>>`，失败达阈值进入 **5 分钟冷却**（冷却结束自动清零，不再永久锁）；同时设置面板加「重新生成配对码」按钮（`reset_pairing()`：换 code + 换 secret + 清失败计数 + 清冷却 + 刷新二维码）。
- 验证：单测 `pair_lockout_cools_down`（直接把 `locked_until` 设成过去时间再配对，应成功）、`reset_pairing_clears_state`。

**B3 更新链路可被装进非 exe** — `update.rs:152/236` + `settings.rs`
- 改法四条：
  1. 资产只认 `.exe` 结尾，删掉 `or_else(|| list.first())` 兜底，找不到就报「这个版本没有可下载的 exe」；
  2. sha256 **强制**：GitHub 的 `digest` 有则必须校验，自定义清单没写 sha256 → 拒绝更新（不再「可选」）；
  3. `update_url` 只接受 `https://`，http 直接拒绝并提示；
  4. 下载完再校验 PE 头（前两字节 `MZ`），防「文件对但根本不是程序」。
- 验证：单测 `non_exe_asset_is_rejected` / `http_update_url_is_rejected` / `missing_sha256_is_rejected` / `non_pe_payload_is_rejected`；现有 `update_roundtrip` 需同步补 sha256 与 MZ 头。
- 风险：中。会让「自己服务器返回不带 sha256 的清单」失效，需在 README 更新源章节写明。

**B4 磁盘配额只是启动时快照** — `config.rs:236/271/278`
- 改法：`quota_left` 语义改为「本次运行已写字节」；507 判定改成上传前现算：`disk_free(dir)` 不足 `DISK_RESERVE + 本批已写` 就 507。`set_upload_dir` 时重置计数。
- 验证：单测 `quota_follows_current_dir`（切换目录后配额按新盘重算）；现有 `upload_quota_is_enforced` 改成按「人为把 quota_left 设小」的方式保留。
- 风险：中。`disk_free` 每次上传调一次 Win32，成本可忽略。

**B5 第二个实例毁掉第一个实例的上传** — `config.rs:517`
- 现状：启动即删 `.td-tmp-*.part`，可能删掉另一个实例正在写的文件；进程内 `UPLOAD_LOCK` 也管不到跨进程重名。
- 改法：启动时先取实例锁（`%LOCALAPPDATA%\TransData\transdata.lock` 的独占文件锁，Windows 用 `CreateFileW` + 无共享），拿不到就弹窗「已经有一个 TransData 在运行」并退出；`cleanup_stale_zip_temps` 只在拿到锁之后执行。
- 验证：单测 `second_instance_cannot_start`（同进程内模拟两次取锁）；手动双开确认第二个窗口提示后退出。

**B6 页面缺 `Vary: Accept-Encoding`** — `server.rs:1116`
- 改法：200 与 304 两条路径都加 `Vary: Accept-Encoding`（304 尤其容易漏）。
- 验证：单测断言两条响应都含该头。

### 批次 C：P2 工程卫生与体验（半天到一天）

| 项 | 内容 |
|---|---|
| C1 | README 补齐 v1.2.0 的新能力：重命名/移动、活动日志、已连接设备、上传原子发布、`/verify` 语义变化、更新源必须 https+sha256 |
| C2 | `PLAN.md` 第 320-390 行是**真实乱码**（不是终端显示问题，文件里就是坏字节）→ 按上下文重写该段 |
| C3 | `transdata.exe.old` 永久残留（运行中 exe 删不掉，`update.rs:268` 静默失败）→ 启动时尝试删 `*.exe.old`，删不掉就留着下次再试 |
| C4 | 便签「远端总是覆盖本地」→ 桌面端有未发送改动时改为「不覆盖 + 顶栏提示『另一端已更新，点击载入』」，避免打字被清空 |
| C5 | 剪贴板：`GlobalLock` 失败仍写空数据 → 释放句柄并返回失败；重试 `sleep` 从 50ms×20 降到 10ms×5，且只在 OpenClipboard 失败时 sleep |
| C6 | 测试卫生：`https_mode_serves_the_api` 硬编码 19876 → 改为绑定 `:0` 取端口；证书写入 `temp_dir` 而不是 target 目录（给 `tls::load_or_create` 加一个可注入目录的重载） |
| C7 | （可选）把 `buildTree` / `buildBatches` / `mergePicked` / `removeSelected` 抽到 `web/logic.js`（UMD 导出），加 `web/test/logic.test.mjs`（node:test，零依赖），给前端补上回归网——A1 那类 bug 就是没有这层网才漏出去的 |

### 批次 D：验收与交付

1. `cargo test`：38 个原用例全绿 + 新增约 12 个（A2/A3/A4/B1/B2/B3/B4/B5/B6）；
2. `cargo build --release`，替换 `target/release/transdata.exe`；
3. e2e 脚本：curl 走 `/info`、`%41` 文件名、25000 文件 verify；Playwright 走 × 按钮、截断提示、配对冷却；
4. 手动：双开实例、HTTPS 下复制地址、便签冲突；
5. 分 6 个 commit：基线 / P0 / WS+配对 / 更新链路 / 配额+实例锁 / 文档+卫生。

### 待确认（批准后开工）

1. **B2 配对锁**：选「5 分钟冷却 + GUI 重置配对码」（推荐），还是只做冷却不做重置按钮？
2. **B3 sha256**：自定义清单不带 sha256 时，是「拒绝更新」（推荐，安全优先）还是「明确警告后仍可继续」？
3. **C4 便签**：改成「不覆盖 + 提示载入」会改变现有交互（现在是远端无条件覆盖），确认要改？
4. **C7**：是否引入 `web/logic.js` + node 测试（多一个静态资源，但前端从此有回归网）？
5. 批次 A 先做还是 A+B 一起做？（建议 A 单独交付一次，先让 P0 上线）

风险总评：A 批全是局部确定性修复，风险最低；B3/B4/B5 会改行为语义，需要 README 同步；C7 是唯一「新增结构」的改动，可以最后再决定。

### 执行记录（2026-10-28，用户批准后实施）

**用户批复**
1. 配对锁 → **冷却 + 重置按钮**（并追问「服务端和客户端知道被锁了吗」→ 客户端知道、桌面端不知道，本轮补上）；
2. sha256 → **警告不拦截**（只挂 GitHub Release，不接受「清单必须带 sha256」的强制要求）；
3. 便签冲突 → **保持覆盖 + 仅提示**（不改交互，但覆盖要说明白）；
4. `web/logic.js` → 确认引入，但要求「成品不能变大」→ 改为编译期拼接进 `/app.js`，请求数和体积都不变；
5. 节奏 → **A+B+C 一起做**，不着急上线。

**已完成**

| 项 | 结果 |
|---|---|
| 0 | 基线提交（977 行）+ `.gitignore` 加 `.workbuddy-ai/` + 版本 1.2.0 |
| A1 | × 按钮改按对象引用删（`L.removeItem`），闭包不再捕获循环变量 |
| A2 | `/info` 按 `tls_on()` 给 http/https；HTTPS 用例里加了断言 |
| A3 | `parse_request` 只解码一次，三个 handler 去掉二次 `url_decode` |
| A4 | `/verify` 改逐条 `symlink_metadata`，删掉「全量扫目录 + 2 万上限」 |
| A5 | `dlTruncated` 接上 UI + `RENDER_CAP` 超限提示（新增 `.listmore`） |
| B1 | WS 便签 4MB 上限 + `WebSocketConfig::max_message_size` |
| B2 | 配对锁 → `pair_locked_until` 冷却 5 分钟；`/pair` 回剩余秒数；`/info` 带 `pair_locked`；桌面端配对卡片显示剩余时间；设置面板加「重新生成配对码」（`reset_pairing()`） |
| B3 | 只认 `.exe` 附件（删掉 `list.first()` 兜底）；`ensure_https`；下载后 `verify_pe` 校验 MZ 头；缺 sha256 时设置面板黄字提示 |
| B4 | `set_upload_dir` 重算配额；每次上传前 `clamp_quota_to_disk`（只收缩） |
| B5 | 新增 `src/instance.rs`（CreateFileW 独占）；`main` 先取锁再清理 |
| B6 | 页面 200/304 都带 `Vary: Accept-Encoding` |
| C1 | README 补齐：重命名/活动记录/设备列表/单实例/更新硬规矩/配对冷却 |
| C2 | PLAN 两段乱码（320-390）按上下文重写并标注「原文不可还原」 |
| C3 | 启动时清 `*.exe.old` |
| C4 | 便签保持覆盖，但状态栏改成「已收到对方内容，本地未发送的改动被覆盖」 |
| C5 | 剪贴板 `GlobalLock` 失败不再写空数据、返回 bool 如实提示；重试 10ms×20；「复制」不再清 `note_local` |
| C6 | HTTPS 用例端口改 `:0` 分配；证书写临时目录（`tls::load_or_create_in`） |
| C7 | `web/logic.js`（UMD）+ `web/test/logic.test.js`（node:test，11 用例）；`src/web.rs::app_js()` 编译期拼接 |

**验证**
- `cargo test`：**50 passed / 0 failed / 5 ignored**（原 38 + 新增 12；ignored 是 4 个 GUI 出图 + 1 个造 2 万文件的 verify 用例）
- `cargo test verify_handles_more_files_than_the_old_cap -- --ignored`：通过（20 500 个文件，45s）
- `node --test web/test/logic.test.js`：**11 passed**
- `cargo build --release` 通过；真 exe 冒烟：`/` 200 + `Vary` 头 + CSP、`/app.js` 44503 字节（logic.js 已拼入）、未授权 `/files` 401
- 未做：便签覆盖后的「撤销」按钮（用户只要提示）；配对冷却时长的可视化倒计时（只在触发请求时刷新）

**遗留（下次再说）**
- `/files` 仍是「全量遍历 + 2 秒缓存」，超大目录首次请求仍慢（A4 只优化了 verify）
- 便签仍是 last-writer-wins，没有冲突合并
- GUI 只有 Windows 实现，非 Windows 分支的剪贴板/单实例是空桩

---

## v1.3.0 计划：管理端（目录列表 + 设备授权 + 设备操作详情）

日期：2026-10-28。用户三轮定调后的最终口径（**先不写代码**，样图见 `mockups/`）。

### 〇、需求口径（用户原话归纳）

1. 桌面端**只有一个列表**，就是这个目录本身——**不叫"传输目录"，也没有上传/下载之分**
   （那是网页端的事）。它是**管理端**，本质是文件管理器；
   之所以要在这里删改，是为了和别的设备共用同一套并发规则（锁、临时文件、撞名拒绝），
   在资源管理器里直接动就失去原子性。
2. **设备管理**：能看、能断开、能**按设备**授权"删除 + 改名"。
   **关掉授权 ≠ 断开**：对方照样能上传、能下载，只是不能删除、不能修改（改名）。
3. **最近操作不是全局流水**，是**每台设备自己的操作详情**，点哪台看哪台。
4. **一屏全放**，不要页签、不要切视图，也不要为了旧窗口尺寸把东西藏起来。

### 一、原子性：管理端不许自己碰文件系统，走同一套 ops

现状（`src/http/server.rs`）：`delete()`（1459 行）和 `rename_path()`（1535 行）里已经写好了
全部并发规则——`UPLOAD_LOCK` 临界区（55 行）、撞名绝不覆盖、只差大小写算同一个文件、
越界路径 `safe_rel` 拒绝、删除后归还配额、`invalidate_files()`、活动日志。
`/files`（681 行）则是一段"DFS 遍历 + 当场拼 JSON"的内联代码。

**改法：把这些从 HTTP handler 里剥出来放 `src/ops.rs`，GUI 和 HTTP 都调它。**

| 新函数 | 说明 |
|---|---|
| `ops::list(cfg) -> Snapshot { entries: Vec<Entry{rel, is_dir, size, mtime}>, truncated }` | 从 `/files` 的 DFS 抽出来，**文件和目录都返回**（GUI 要拼树）；`/files` 只把文件条目序列化成 JSON，网页端行为不变 |
| `ops::delete(cfg, rel, who) -> Result<DeleteSummary, String>` | 从 `delete()` 抽核心，返回「删了什么、多大、几个文件」，GUI 用它写 toast |
| `ops::rename(cfg, from, to, who) -> Result<(), String>` | 从 `rename_path()` 抽核心（建父目录、canonicalize 校验、锁内 rename） |

- `who` 传 `"本机"`（管理端）或 peer IP，活动日志能分清是谁动的。
- HTTP handler 变薄壳：读 body → 调 ops → 写 JSON，错误串原样返回，**行为不变**。
- GUI 只调 ops，**绝不出现 `fs::remove_file` / `fs::rename`**。这样管理端和别的设备
  对同一个文件的操作排在同一把锁上：不会删掉别人正在写的目标，也不会两边改名互相覆盖。

**风险**：纯搬运，但 `delete()` 现在直接往 socket 写中文错误串，要改成 `Result<String, String>`；
3 个现有用例（`delete_removes_directory_tree` / `delete_returns_quota` /
`rename_moves_and_refuses_overwrite`）必须一条不改地全绿，作为"搬运没走形"的证据。

**诚实边界**：只保证"通过本程序的操作"之间原子；用户仍然可以拿资源管理器动同一个目录
（Windows 权限模型决定的，程序管不了）。所以工具栏保留「在资源管理器中打开」，
但默认引导用户在这个列表里操作。

### 二、界面：一屏三栏（1200×760，不迁就旧尺寸）

```
┌ 标题栏  TransData v1.3.0       运行中 · 端口 9999 · 3 台设备       🌙 ⚙ ─ □ × ┐
├──────────────────────────┬────────────────┬────────────────────────────┤
│ 📁 D:\TransData\uploads [刷新][打开目录]│ 连接  未配对设备看不到文件│ 192.168.1.23        [可删改] │
│ 名称        大小  修改时间 │ ┌────┐ 配对码   │ iPhone·Safari·刚刚          │
│ ▾ 📁 归档   2 个文件 … ✏🗑│ │ QR │ 48210396 │ 本次 18 分钟 · ↑3 · ↓5     │
│   📄 报告.pdf 2.4MB …  ✏🗑│ └────┘ 192.168… │ 操作 / 文件      大小    时间 │
│ ▸ 📁 备份   2 个文件 … ✏🗑│ ── HTTPS 加密 [○]│ ✕ 旧备份.zip    88 MB  8 分钟│
│ …                        │ 设备 3 台       │ ↑ 归档/报告.pdf 2.4MB  1 小时│
│ ──────────────────────── │ 设备 空闲 传输 删除/修改│ ↓ 备份/照片.zip  118MB 2 小时│
│ 已选中：报告.pdf · 2.4 MB │ ● .23 刚刚 ↑↓ [●] 断开│ ✎ 会议记录.txt   4 KB 21 分钟│
│                          │ ● .57 3分  ↑↓ [○] 断开│ …（一屏 20 条）             │
│                          │ ● .88 26分 ↑↓ [●] 断开│                            │
│                          │ ＋ 等待新设备接入    │                            │
├──────────────────────────┴────────────────┴────────────────────────────┤
│ 快捷文本  [______________________________________]  [复制] [发送]        │
└────────────────────────────────────────────────────────────────────────┘
```

- **左｜目录（516px，flex 吃掉剩余）**：工具栏 = 路径框（等宽字体）+ 刷新 + 打开目录；
  列头 `名称 / 大小 / 修改时间`；**树形**（文件夹 ▸ 展开，子项缩进 14px，文件夹排在文件前）；
  行内 ✏ 🗑；底部状态栏 `已选中：…` + `N 个文件夹 · M 个文件 · 总大小`。行高 **30px**。
- **中｜连接 + 设备（296 固定）**：上面是紧凑的「连接」面板（一行标题 + `QR 86` 与配对码/地址/复制地址并排，
  分隔线下面一行 **启用 HTTPS（加密传输）** 开关 + 一句说明；HTTPS 打开时地址变 `https://` 并多一行证书指纹）。
  **二维码不再上下撑开**，面板高度只跟内容走。
  下面是设备表：表头 `设备 / 空闲 / 传输 / 删除/修改`，每行 `● IP ｜ 空闲 ｜ ↑↓ ｜ 开关 ｜ 断开`，
  **一台设备只占 30px**（旧版一张卡 104px，3 台差 220px）。`传输` 列永远是绿的（上传下载不受授权影响），
  `删除/修改` 列才是开关；表尾一行虚线「＋ 等待新设备接入」，剩下的高度留在列表里（设备多了直接往下排，不挤压别的块）。
- **右｜设备操作详情（336 固定，一条一行）**：头部 `IP + 权限标记`、`型号 · 空闲`、
  `本次 N 分钟 · ↑x · ↓y`；表头 `操作 / 文件 ｜ 大小 ｜ 时间`（大小、时间**横着排**，
  不再叠在文件名下面）；行高 **26px**，一屏 20 条。
- **底部｜便签条 76**：原来 202px 的大卡压成一条，但输入框**不能窄成一条线**——两行高度（44px 的 textarea）+ 复制/发送，粘贴长文本能看见两行。
- **窗口 640×470 → 1200×760**；最小尺寸 640×470 时右两栏收起，标题栏出现 `设备 (3)` 抽屉按钮。
- **尺寸是算出来的**：内宽 1168 = 目录 516 + 设备 296 + 详情 336 + 两处 10px 间距；
  纵向 694 = 便签 76 + 10 + 主区 608；主区里目录 = 工具栏 38 + 列头 22 + 列表 520 + 状态栏 24
  （520/30 ≈ 17 行文件）。实测三栏 clientHeight 与 scrollHeight 全部相等
  （目录 520 / 设备列表 354 / 详情 498），无一处溢出。
- 样图：`mockups/desktop-v1.3.html`；截图 `preview-main.png` / `preview-device.png` /
  `preview-rename.png` / `preview-delete.png`。

### 三、设备管理：按设备授权（不是全局开关）

`config.rs` 的 `sessions`（148 行）现在只存 `ip + at`。改成：

```rust
struct Session { at: Instant, ip: String, allow_write: bool }   // 默认 true

struct DeviceInfo { ip: String, idle_secs: u64, sessions: usize, allow_write: bool }
fn device_list(&self) -> Vec<DeviceInfo>         // 活跃在前，按 IP 去重
fn set_device_write(&self, ip: &str, on: bool)   // 写该 IP 下所有会话
fn session_can_write(&self, sid: &str) -> bool   // 校验用
fn drop_client(&self, ip: &str) -> usize         // 踢下线，返回删了几条会话
```

- **同一 IP 的多个标签页算一台设备**，授权对它们一起生效（和现有 `session_peers()` 同口径）。
- 新配对设备默认允许；**不落盘**，重启全部恢复允许（对应"默认打开"）。

**服务端怎么落地（前端置灰不算权限）**
- 校验放路由层（`handle_conn` 的 match，768~778 行）：`DELETE /delete/*`、`POST /rename`
  先取会话 → `session_can_write(sid) == false` 就
  `403 {"success":false,"error":"这台设备已被禁止删除 / 修改"}`。
- **管理端自己走 `X-TD-Secret` 的请求豁免**（`authorized()` 413 行本来就能区分两条路），
  否则关掉授权后自己也没法删改。
- `/info` 增加 `"can_write"`（按请求方自己那台设备算）；网页据此把「删除」「重命名」置灰 +
  黄条「电脑端已禁止这台设备删除 / 修改，上传下载不受影响」，点了也不发请求。
- **上传、下载、打包一律不看这个开关**——"关授权不等于断开"就是这条。

**设备操作详情**：`config.rs` 已有 `recent_activity_for(ip, n)`（`Activity` 环形缓冲，含
ip/kind/name/size），右栏直接用；桌面端原来的全局「最近操作」一行去掉。

### 四、目录列表：树形 + 后台刷新（不能卡 UI）

- GUI 拿 `ops::list` 的扁平条目自己拼树（父路径缺失就补节点，文件夹在前按名排序）。
- 折叠状态只在 GUI 内存里（`HashSet<rel>`）；删改后父目录自动展开，避免"改完看不见"。
- 条目上限 10000（和 `/files` 一致），渲染上限 500 行，超出提示"仅渲染前 500 行"。
- 刷新：后台线程跑 `ops::list` → `mpsc` 回主线程（`update`/`/info` 同一个模式），
  3 秒节流，最小化不刷；加载中工具栏显示"正在读取…"。
- 选中项：单击选中（浅紫底），状态栏显示名字 + 大小；文件夹单击同时切展开。

### 五、删除 / 修改 / 断开的交互

- **改名**：点铅笔 → 名字原地变输入框，**只选中主文件名**（不含扩展名），`Enter` 提交、
  `Esc` 取消；文件夹也能改名。失败在状态栏红字给原因（撞名 / 正被占用 / 名字不合法）。
- **删除**：点垃圾桶 → 居中确认窗；文件写「不可撤销，配额立即归还」，
  文件夹写「文件夹和里面的 N 个项目会一起删除」。
- **断开**：设备卡上的按钮直接踢（对方下次请求 401 → 网页已有 401 处理，回配对页）；
  可恢复操作，不做二次确认，但给一条 toast。
- 每次删改后：`invalidate_files()` + 记活动日志（who = 本机）+ 立刻重取列表 + 右栏详情刷新。

### 六、测试与验收

| 层 | 用例 |
|---|---|
| ops 搬运 | 现有 3 个 delete/rename 用例原样全绿；新增 `ops_list_includes_dirs`、`ops_list_matches_files_endpoint`（文件条目数/大小与 `/files` 一致） |
| 并发 | `desktop_ops_are_atomic_with_upload`：一边写 `.td-tmp-*.part`，一边 `ops::delete` 同目标 → 只能「删掉」或「报占用」，不留半个文件 |
| 授权 | `device_write_flag_is_per_device`：A 关授权 → A 的 cookie `DELETE` 403、B 的 cookie 200、`X-TD-Secret` 200；`/info` 的 `can_write` 各自正确 |
| 授权不影响传输 | `write_flag_does_not_block_upload`：A 关授权后仍能 `/upload` 成功、`/download` 200 |
| 断开 | `drop_client_kills_its_sessions`：`session_count` 归零、旧 sid 失效、`device_list` 少一台 |
| GUI 无头 | 三栏（树展开态 / 设备开关开与关 / 详情流水）、改名输入框、删除确认框；1200×760 不溢出，640×470 走抽屉降级 |
| 前端 | Playwright：关授权 → 网页两个按钮置灰 + 黄条 + 不发请求，上传下载仍可用；`node --test` 不受影响 |

交付：`cargo test` 全绿（50 + 新增约 7）→ `node --test` → `cargo build --release` →
真 exe 冒烟（管理端删文件、网页同步消失；关某台授权、它删不掉但仍能传）→
README 补两节（管理端目录 / 设备授权）。

### 七、定稿（2026-10-28，样图已确认）

**术语（界面文案就按这个写，不要再用缩写）**

| 词 | 含义 |
|---|---|
| **删除 / 修改** | 授权开关的标签。管两件事：`DELETE /delete/*` 和 `POST /rename`（改名属于"修改"）。开关默认**开** |
| **传输** | 上传 + 下载 + 打包下载。**永远可用**，不受授权开关影响；设备表里那一列恒为绿色 `↑↓` |
| **断开** | 踢掉该设备的会话，对方要重新扫码/输码。和授权是两件独立的事 |

**已定的 5 个细节**（原"待确认"，按推荐值执行，要改随时说）

1. **窄窗口**：最小 640×470 时右两栏收起，标题栏出现 `设备 (3)` 抽屉按钮（三栏在窄屏放不下，不做横向滚动）。
2. **不做"全部禁止"总开关**：每台设备各一个开关，不额外加全局开关。
3. **设备按 IP 认**：同一 IP 的多个标签页算一台；DHCP 换 IP 会当成新设备（授权要重设，这是已知代价）。
4. **断开不二次确认**：可恢复操作，踢完给一条 toast「已断开 x.x.x.x，对方需要重新配对」。
5. **授权状态不落盘**：重启后所有设备恢复"允许"（对应"默认打开"）。

**实施顺序（每步都要能独立验证，不一次性铺开）**

| 步 | 内容 | 验证 |
|---|---|---|
| 1 | 建分支 + 版本号 1.3.0 + 现有工作区提交干净 | `cargo test` 50 绿 |
| 2 | `src/ops.rs`：`list / delete / rename` 从 server.rs 抽出来 | 3 个旧用例一条不改全绿 |
| 3 | `config.rs`：`Session.allow_write` + `device_list / set_device_write / session_can_write / drop_client` | 新增 4 个单测 |
| 4 | `server.rs`：路由层 403 + `/info` 的 `can_write` | `device_write_flag_is_per_device`、`write_flag_does_not_block_upload` |
| 5 | 网页端：`can_write=false` 时两个按钮置灰 + 黄条 | Playwright 走一遍 |
| 6 | `gui.rs`：三栏重排（连接面板 / 设备表 / 详情表 / 文件树 / 便签条 76px）+ 后台刷新 + 行内改名 + 删除确认 | 无头渲染 + 真机点一遍 |
| 7 | 收尾：README 两节、PLAN 执行记录、`cargo build --release` | 真 exe 冒烟 |

**样图**：`mockups/desktop-v1.3.html`（可点），截图 `preview-main.png` / `preview-https.png` /
`preview-device.png` / `preview-rename.png` / `preview-delete.png`。

风险总评：ops 抽取是纯搬运，风险低但必须用现有测试兜住；按设备授权是新增校验点，
只加在两条路由上、管理端豁免，回归面小；真正的成本在 GUI——三栏重排 + 树形列表 +
后台刷新 + 行内编辑 + 确认窗，约等于一次中等规模重写。

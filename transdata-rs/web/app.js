/* ==========================================================================
   TransData · Web UI
   - 全部事件用 addEventListener 绑定（无内联 handler）→ 可用严格 CSP
   - 所有用户数据（文件名等）一律用 textContent 写入，不拼 innerHTML
   - 认证走 HttpOnly 会话 Cookie，页面里不出现任何令牌
   ========================================================================== */
(function () {
    'use strict';

    var MODE = document.body.dataset.mode || 'app';

    var $ = function (id) { return document.getElementById(id); };
    var MODE_APP = MODE === 'app';
    /* 纯逻辑在 /logic.js（编译期拼在本文件前面下发），见 web/logic.js */
    var L = self.TDLogic;
    /* initTabs 里赋成切页函数：粘贴截图后要自动跳到「上传」看到进度 */
    var tabSelect = null;

    /* ---------------------------------------------------------------- utils */

    function fmtSize(bytes) { return L.fmtSize(bytes); }

    function fmtDuration(sec) {
        if (!isFinite(sec) || sec <= 0) return '';
        if (sec < 60) return Math.ceil(sec) + ' 秒';
        if (sec < 3600) return Math.ceil(sec / 60) + ' 分钟';
        return (sec / 3600).toFixed(1) + ' 小时';
    }

    function fmtTime(ts) {
        var d = new Date(ts), p = function (n) { return (n < 10 ? '0' : '') + n; };
        return p(d.getHours()) + ':' + p(d.getMinutes()) + ':' + p(d.getSeconds());
    }

    function el(tag, cls, text) {
        var n = document.createElement(tag);
        if (cls) n.className = cls;
        if (text != null) n.textContent = text;
        return n;
    }

    function icon(path) {
        var svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
        svg.setAttribute('viewBox', '0 0 24 24');
        svg.setAttribute('width', '16');
        svg.setAttribute('height', '16');
        svg.setAttribute('fill', 'none');
        svg.setAttribute('stroke', 'currentColor');
        svg.setAttribute('stroke-width', '1.7');
        svg.setAttribute('stroke-linecap', 'round');
        svg.setAttribute('stroke-linejoin', 'round');
        svg.setAttribute('aria-hidden', 'true');
        var p = document.createElementNS('http://www.w3.org/2000/svg', 'path');
        p.setAttribute('d', path);
        svg.appendChild(p);
        return svg;
    }

    var ICON_FILE = 'M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8l-5-5Z M14 3v5h5';
    var ICON_FOLDER = 'M4 7.5 12 3l8 4.5v9L12 21l-8-4.5v-9Z m8 4.5 8-4.5';
    var ICON_TRASH = 'M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13M10 11v6M14 11v6';
    var ICON_PENCIL = 'M12 20h9 M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4 12.5-12.5Z';

    /* -------------------------------------------------------------- network */

    function api(path, opts, keepOn401) {
        opts = opts || {};
        opts.credentials = 'same-origin';
        return fetch(path, opts).then(function (res) {
            if (res.status === 401 && !keepOn401) {
                // 会话失效（服务重启 / 授权被撤销）→ 回到配对页
                location.reload();
                throw new Error('unauthorized');
            }
            return res;
        });
    }

    function apiJson(path, opts) {
        return api(path, opts).then(function (res) {
            return res.json().catch(function () { return {}; });
        });
    }

    function setConn(ok, text) {
        var pill = $('connPill');
        if (!pill) return;
        pill.classList.toggle('is-down', !ok);
        $('connText').textContent = text;
    }

    function notice(node, msg, kind) {
        if (!node) return;
        if (!msg) { node.hidden = true; node.textContent = ''; node.className = 'notice'; return; }
        node.hidden = false;
        node.className = 'notice notice--' + (kind || 'info');
        node.textContent = msg;
    }

    /* ==================================================================== */
    /*  配对视图                                                             */
    /* ==================================================================== */

    function initPair() {
        $('pairView').hidden = false;
        $('appView').hidden = true;

        var form = $('pairForm');
        var input = $('pairCode');
        var msg = $('pairMsg');
        var btn = $('pairSubmit');

        function say(text, ok) {
            msg.textContent = text;
            msg.classList.toggle('is-ok', !!ok);
        }

        function submit(value) {
            var v = (value || '').replace(/\D/g, '');
            if (v.length !== 8) { say('请输入 8 位配对码'); input.focus(); return; }
            btn.disabled = true;
            say('正在连接…', true);
            api('/pair', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({ code: v })
            }, true).then(function (res) {
                return res.json().catch(function () { return {}; })
                    .then(function (j) { return { ok: res.ok, j: j }; });
            }).then(function (r) {
                if (r.ok && r.j.success) {
                    say('配对成功', true);
                    location.href = '/';
                } else {
                    btn.disabled = false;
                    say(r.j.error || '配对失败，请确认配对码');
                    input.select();
                }
            }).catch(function () {
                btn.disabled = false;
                say('无法连接服务器');
            });
        }

        form.addEventListener('submit', function (e) {
            e.preventDefault();
            submit(input.value);
        });

        input.addEventListener('input', function () {
            input.value = input.value.replace(/\D/g, '').slice(0, 8);
        });

        // 扫码进入时 URL 片段里带密钥（#k=…），片段不会发到服务器，
        // 直接用它换一张会话 Cookie。
        var hash = location.hash || '';
        var m = hash.match(/[#&]k=([0-9a-fA-F]{16,})/);
        if (m) {
            history.replaceState(null, '', location.pathname);
            btn.disabled = true;
            say('正在通过二维码配对…', true);
            api('/pair', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({ secret: m[1] })
            }, true).then(function (res) {
                if (res.ok) { location.href = '/'; return; }
                btn.disabled = false;
                say('二维码已失效，请重新扫描');
            }).catch(function () {
                btn.disabled = false;
                say('无法连接服务器');
            });
        } else {
            input.focus();
        }
    }

    /* ==================================================================== */
    /*  主界面                                                              */
    /* ==================================================================== */

    function initApp() {
        $('pairView').hidden = true;
        $('appView').hidden = false;

        initTabs();
        initUpload();
        initPaste();
        initDownload();
        initNote();
        initServerInfo();
        initDialog();
    }

    /* ------------------------------------------------------------- 弹窗 */

    /* 改名 / 删除确认用页面内自绘的弹窗，不用原生 prompt / confirm：
       部分手机浏览器（还有 App 内嵌 WebView）会把系统弹窗直接吞掉，
       prompt 返回空、confirm 返回 false，点按钮看起来就是「没反应」。 */

    var dlg = null;   // { mode, resolve, opener }

    function initDialog() {
        var modal = $('opModal');
        if (!modal) return;
        $('opCancel').addEventListener('click', function () { closeDialog(null); });
        $('opOk').addEventListener('click', function () { submitDialog(); });
        // 点遮罩关闭；点面板内部不关
        modal.addEventListener('click', function (e) {
            if (e.target === modal) closeDialog(null);
        });
        $('opInput').addEventListener('keydown', function (e) {
            if (e.key === 'Enter') { e.preventDefault(); submitDialog(); }
        });
        document.addEventListener('keydown', function (e) {
            if (dlg && e.key === 'Escape') { e.preventDefault(); closeDialog(null); }
        });
    }

    /* opts: { title, text?, input?, okLabel?, danger? } → Promise
       带 input 时返回输入串（取消为 null），否则返回 true / null */
    function openDialog(opts) {
        return new Promise(function (resolve) {
            dlg = { mode: opts.input != null ? 'input' : 'confirm', resolve: resolve, opener: document.activeElement };
            $('opTitle').textContent = opts.title || '';
            var text = $('opText');
            text.textContent = opts.text || '';
            text.hidden = !opts.text;
            var ok = $('opOk');
            ok.textContent = opts.okLabel || '确定';
            ok.className = 'btn ' + (opts.danger ? 'btn--danger-solid' : 'btn--primary');
            var input = $('opInput');
            input.hidden = opts.input == null;
            input.value = opts.input || '';
            $('opModal').hidden = false;
            if (opts.input != null) {
                input.focus();
                // 选中主名、保留扩展名：改个名不用先删掉 .epub
                var slash = Math.max(input.value.lastIndexOf('/'), input.value.lastIndexOf('\\'));
                var dot = input.value.lastIndexOf('.');
                var end = dot > slash + 1 ? dot : input.value.length;
                try { input.setSelectionRange(slash + 1, end); } catch (e) { input.select(); }
            } else {
                ok.focus();
            }
        });
    }

    function submitDialog() {
        if (!dlg) return;
        var isInput = dlg.mode === 'input';
        closeDialog(isInput ? $('opInput').value.trim() : true);
    }

    function closeDialog(value) {
        var d = dlg;
        if (!d) return;
        dlg = null;
        $('opModal').hidden = true;
        if (d.opener && d.opener.focus) d.opener.focus();
        d.resolve(value);
    }

    /* ------------------------------------------------------------------ tabs */

    function initTabs() {
        var tabs = [
            { btn: $('tabUpload'), panel: $('uploadPanel') },
            { btn: $('tabDownload'), panel: $('downloadPanel') }
        ];

        function select(idx) {
            tabs.forEach(function (t, i) {
                var on = (i === idx);
                t.btn.classList.toggle('is-active', on);
                t.btn.setAttribute('aria-selected', on ? 'true' : 'false');
                t.panel.hidden = !on;
            });
        }
        tabSelect = select;

        tabs.forEach(function (t, i) {
            t.btn.addEventListener('click', function () { select(i); });
        });
    }

    /* ----------------------------------------------------------- 服务器信息 */

    function initServerInfo() {
        var addr = location.origin;

        $('copyAddr').addEventListener('click', function () {
            var btn = this;
            copyText(addr).then(function (ok) {
                btn.textContent = ok ? '已复制' : '复制失败';
                setTimeout(function () { btn.textContent = '复制地址'; }, 1400);
            });
        });

        apiJson('/info').then(function (j) {
            if (j && j.url) {
                addr = j.url;
                $('copyAddr').setAttribute('aria-label', '复制访问地址 ' + addr);
            }
            applyCanWrite(j && j.can_write);
            setConn(true, '已连接');
        }).catch(function () {
            setConn(false, '连接断开');
        });
    }

    /* 电脑端可以按设备关掉「删除 / 修改」。置灰只是提示，真正的判定在服务端
       （403）；点了也不发请求，免得白跑一趟网络。 */
    function applyCanWrite(ok) {
        var next = ok !== false;
        if (next === canWrite) return;
        canWrite = next;
        var banner = $('roNotice');
        if (banner) banner.hidden = canWrite;
        renderFiles();
    }

    /* 操作完 / 切页签时重新问一次：授权可能在手机上停留期间被改掉 */
    function refreshCanWrite() {
        return apiJson('/info').then(function (j) {
            applyCanWrite(j && j.can_write);
        }).catch(function () { /* 网络断了另有提示 */ });
    }

    function copyText(text) {
        if (navigator.clipboard && window.isSecureContext) {
            return navigator.clipboard.writeText(text).then(function () { return true; })
                .catch(function () { return legacyCopy(text); });
        }
        return Promise.resolve(legacyCopy(text));
    }

    function legacyCopy(text) {
        var ta = document.createElement('textarea');
        ta.value = text;
        ta.setAttribute('readonly', '');
        ta.className = 'code-input';
        ta.style.position = 'fixed';
        ta.style.opacity = '0';
        document.body.appendChild(ta);
        ta.select();
        var ok = false;
        try { ok = document.execCommand('copy'); } catch (e) { ok = false; }
        document.body.removeChild(ta);
        return ok;
    }

    /* ------------------------------------------------------------- 上传 */

    var selected = [];      // [{file, path, thumbUrl?}]
    var uploading = false;
    /* 这一批开始时选中的列表快照：传输期间新加的文件不能被当成「已传完」清掉 */
    var uploadSnapshot = [];
    var autoAfter = false;  // 传输中又粘了图：这一批传完自动接着传
    var uploadXhr = null;
    var uploadAbort = false;
    var BATCH_MAX_FILES = 32;
    var BATCH_MAX_BYTES = 256 * 1024 * 1024;
    var RENDER_CAP = 200;
    /* 折叠状态下显示 8 行，超出才出现「展开」；展开后列表自己滚动 —— 页面永远不会被撑长 */
    var UPLOAD_COLLAPSED_ROWS = 8;
    var DL_COLLAPSED_ROWS = 8;
    var filesExpanded = false;
    var dlExpanded = false;

    function initUpload() {
        var dz = $('dropzone');
        var fileInput = $('fileInput');
        var folderInput = $('folderInput');
        var photoInput = $('photoInput');

        $('pickPhotos').addEventListener('click', function (e) { e.stopPropagation(); photoInput.click(); });
        $('pickFiles').addEventListener('click', function (e) { e.stopPropagation(); fileInput.click(); });
        $('pickFolder').addEventListener('click', function (e) { e.stopPropagation(); folderInput.click(); });

        dz.addEventListener('click', function () { fileInput.click(); });
        dz.addEventListener('keydown', function (e) {
            if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); fileInput.click(); }
        });

        // 相册入口：accept=image/* 让手机走系统照片选择器（缩略图网格、可多选），
        // 而不是先把人丢进文件管理器的「最近文件」里翻照片。
        photoInput.addEventListener('change', function () {
            var picked = [];
            for (var i = 0; i < photoInput.files.length; i++) {
                var f = photoInput.files[i];
                picked.push({ file: f, path: f.name });
            }
            photoInput.value = '';
            mergePicked(picked);
            renderSelected();
        });

        fileInput.addEventListener('change', function () {
            var picked = [];
            for (var i = 0; i < fileInput.files.length; i++) {
                var f = fileInput.files[i];
                picked.push({ file: f, path: f.name });
            }
            fileInput.value = '';
            mergePicked(picked);
            renderSelected();
        });

        folderInput.addEventListener('change', function () {
            var picked = [];
            var flat = true;
            for (var i = 0; i < folderInput.files.length; i++) {
                var f = folderInput.files[i];
                var rel = f.webkitRelativePath || f.name;
                if (rel.indexOf('/') >= 0) flat = false;
                picked.push({ file: f, path: rel.replace(/\\/g, '/') });
            }
            folderInput.value = '';
            if (picked.length > 0 && flat) {
                notice($('uploadNotice'),
                    '当前浏览器不支持选择文件夹（手机浏览器都受限），已按普通文件处理。' +
                    '文件较多时建议先在手机里压缩成 zip 再传。', 'warn');
            }
            mergePicked(picked);
            renderSelected();
        });

        // 拖拽
        ['dragenter', 'dragover'].forEach(function (ev) {
            dz.addEventListener(ev, function (e) {
                e.preventDefault(); e.stopPropagation();
                dz.classList.add('is-over');
            });
        });
        ['dragleave', 'drop'].forEach(function (ev) {
            dz.addEventListener(ev, function (e) {
                e.preventDefault(); e.stopPropagation();
                if (ev === 'dragleave' && dz.contains(e.relatedTarget)) return;
                dz.classList.remove('is-over');
            });
        });
        dz.addEventListener('drop', function (e) {
            var dt = e.dataTransfer;
            if (!dt) return;
            var items = dt.items ? dt.items : null;
            var picked = [];
            var entries = [];
            if (items && items.length && items[0].webkitGetAsEntry) {
                for (var i = 0; i < items.length; i++) {
                    var en = items[i].webkitGetAsEntry && items[i].webkitGetAsEntry();
                    if (en) entries.push(en);
                }
            }
            if (entries.length) {
                Promise.all(entries.map(readEntry)).then(function (res) {
                    picked = res.flat();
                    mergePicked(picked);
                    renderSelected();
                });
            } else if (dt.files) {
                for (var j = 0; j < dt.files.length; j++) {
                    picked.push({ file: dt.files[j], path: dt.files[j].name });
                }
                mergePicked(picked);
                renderSelected();
            }
        });

        $('uploadBtn').addEventListener('click', function () { startUpload(); });
        $('cancelUpload').addEventListener('click', function () {
            uploadAbort = true;
            if (uploadXhr) uploadXhr.abort();
        });

        $('toggleFiles').addEventListener('click', function () {
            filesExpanded = !filesExpanded;
            renderSelected();
        });
    }

    /* 页面里 Ctrl+V 粘贴截图：浏览器把剪贴板图片给成一个 File，直接进上传列表。
       只有「纯图片」才拦截 —— 带文字的剪贴板照常粘进便签/输入框，文字优先。 */
    function initPaste() {
        document.addEventListener('paste', function (e) {
            var dt = e.clipboardData;
            if (!dt) return;
            var text = '';
            try { text = dt.getData('text/plain') || ''; } catch (err) { text = ''; }
            var files = [];
            if (dt.items) {
                for (var i = 0; i < dt.items.length; i++) {
                    var item = dt.items[i];
                    if (item.kind === 'file' && item.type && item.type.indexOf('image/') === 0) {
                        var f = item.getAsFile();
                        if (f) files.push(f);
                    }
                }
            }
            if (!files.length && dt.files) {
                for (var j = 0; j < dt.files.length; j++) {
                    if (isImage(dt.files[j])) files.push(dt.files[j]);
                }
            }
            if (!files.length || text.trim()) return;

            e.preventDefault();
            var d = new Date();
            var extOf = { 'image/jpeg': 'jpg', 'image/gif': 'gif', 'image/webp': 'webp', 'image/bmp': 'bmp' };
            var used = {};
            selected.forEach(function (x) { used[x.path] = true; });
            var picked = [];
            files.forEach(function (f) {
                var ext = extOf[f.type] || 'png';
                var seq = 1;
                var name = L.shotName(d, seq, ext);
                while (used[name]) { seq++; name = L.shotName(d, seq, ext); }
                used[name] = true;
                picked.push({ file: f, path: name });
            });

            mergePicked(picked);
            if (tabSelect) tabSelect(0);
            renderSelected();
            if (uploading) {
                autoAfter = true;
                notice($('uploadNotice'), '已加入 ' + picked.length + ' 张截图，当前传输完成后自动上传。', 'ok');
            } else {
                notice($('uploadNotice'), '已加入 ' + picked.length + ' 张截图，开始上传…', 'info');
                startUpload();
            }
        });
    }

    /* 递归读取拖入的目录（Chrome/Edge 支持 webkitGetAsEntry） */
    function readEntry(entry) {
        if (entry.isFile) {
            return new Promise(function (resolve) {
                entry.file(function (f) {
                    resolve([{ file: f, path: entry.fullPath.replace(/^\//, '') }]);
                }, function () { resolve([]); });
            });
        }
        if (entry.isDirectory) {
            var reader = entry.createReader();
            return new Promise(function (resolve) {
                reader.readEntries(function (children) {
                    Promise.all(children.map(readEntry)).then(function (lists) {
                        resolve(lists.flat());
                    });
                }, function () { resolve([]); });
            });
        }
        return Promise.resolve([]);
    }

    function mergePicked(picked) {
        selected = L.mergePicked(selected, picked);
    }

    function isImage(file) {
        return !!(file && file.type && file.type.indexOf('image/') === 0);
    }

    /* 列表里的图片预览：一个文件一个 object URL，移除/传完后释放 */
    function thumbUrl(it) {
        if (!it.thumbUrl) it.thumbUrl = URL.createObjectURL(it.file);
        return it.thumbUrl;
    }

    function releaseThumb(it) {
        if (it && it.thumbUrl) {
            try { URL.revokeObjectURL(it.thumbUrl); } catch (e) { }
            it.thumbUrl = null;
        }
    }

    function renderSelected() {
        var list = $('fileList');
        list.textContent = '';

        var showCount = selected.length;
        if (!filesExpanded && showCount > UPLOAD_COLLAPSED_ROWS) showCount = UPLOAD_COLLAPSED_ROWS;
        if (showCount > RENDER_CAP) showCount = RENDER_CAP;

        for (var idx = 0; idx < showCount; idx++) {
            var it = selected[idx];
            var li = el('li');

            if (isImage(it.file)) {
                // 缩略图：从相册/文件夹里选了一堆 IMG_xxx 时，靠它认出是哪张
                var thumb = el('span', 'fi-thumb');
                var img = el('img');
                img.src = thumbUrl(it);
                img.alt = '';
                img.decoding = 'async';
                img.loading = 'lazy';
                thumb.appendChild(img);
                li.appendChild(thumb);
            } else {
                var ic = el('span', 'fi-icon');
                ic.appendChild(icon(ICON_FILE));
                li.appendChild(ic);
            }

            var main = el('div', 'fi-main');
            main.appendChild(el('div', 'fi-name', it.path));
            li.appendChild(main);
            li.appendChild(el('span', 'fi-sub', fmtSize(it.file.size)));

            var actions = el('div', 'fi-actions');
            var rm = el('button', 'icon-btn');
            rm.type = 'button';
            rm.title = '移除';
            rm.setAttribute('aria-label', '移除 ' + it.path);
            rm.textContent = '×';
            // 必须按「对象引用」删，不能用循环下标：这里显示的行号是折叠后的
            // 行号（最多 8 行），跟 selected 的下标不是一回事。
            rm.addEventListener('click', (function (item) {
                return function () {
                    releaseThumb(item);
                    selected = L.removeItem(selected, item);
                    renderSelected();
                };
            })(it));
            actions.appendChild(rm);
            li.appendChild(actions);

            list.appendChild(li);
        }

        list.classList.toggle('is-scrollable', filesExpanded && selected.length > UPLOAD_COLLAPSED_ROWS);

        var total = 0;
        selected.forEach(function (x) { total += x.file.size; });

        // 摘要行 + 展开/收起（不显示"还有 N 个"，一个展开按钮就够了）
        $('uploadListBar').hidden = selected.length === 0;
        $('uploadSummary').textContent = selected.length ? '共 ' + fmtSize(total) : '';
        var tg = $('toggleFiles');
        tg.hidden = selected.length <= UPLOAD_COLLAPSED_ROWS;
        tg.textContent = filesExpanded ? '收起' : '展开';
        tg.setAttribute('aria-expanded', filesExpanded ? 'true' : 'false');

        $('uploadChip').textContent = selected.length ? selected.length + ' 个文件' : '未选择文件';
        $('uploadBtn').disabled = selected.length === 0 || uploading;
        $('uploadBtn').textContent = uploading ? '传输中…' : '开始上传';
    }

    function manifest(items) {
        return items.map(function (it) { return { path: it.path, size: it.file.size }; });
    }

    function verifyManifest(entries) {
        var chunks = [];
        for (var i = 0; i < entries.length; i += 15000) chunks.push(entries.slice(i, i + 15000));
        var chain = Promise.resolve({ all_ok: true, results: [] });
        chunks.forEach(function (c) {
            chain = chain.then(function (acc) {
                return apiJson('/verify', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({ files: c })
                }).then(function (j) {
                    if (j && j.all_ok === false) acc.all_ok = false;
                    acc.results = acc.results.concat(j.results || []);
                    return acc;
                });
            });
        });
        return chain;
    }

    function uploadBatch(batch, onProgress) {
        return new Promise(function (resolve, reject) {
            var fd = new FormData();
            batch.forEach(function (it) { fd.append('file', it.file, it.path); });
            var xhr = new XMLHttpRequest();
            uploadXhr = xhr;
            xhr.open('POST', '/upload');
            xhr.withCredentials = true;
            xhr.upload.onprogress = function (e) {
                if (e.lengthComputable) onProgress(e.loaded);
            };
            xhr.onload = function () {
                uploadXhr = null;
                var j = {};
                try { j = JSON.parse(xhr.responseText); } catch (err) { j = {}; }
                if (xhr.status === 401) { location.reload(); reject(new Error('unauthorized')); return; }
                resolve(j);
            };
            xhr.onerror = function () { uploadXhr = null; reject(new Error('网络错误')); };
            xhr.onabort = function () { uploadXhr = null; reject(new Error('已取消')); };
            xhr.send(fd);
        });
    }

    function setProgress(done, total, batchIndex, batchCount) {
        var pct = total > 0 ? Math.min(100, (done / total) * 100) : 0;
        $('progBar').style.width = pct.toFixed(1) + '%';
        var now = Date.now();
        var secs = (now - progressT0) / 1000;
        var speed = secs > 0.3 ? done / secs : 0;
        var left = speed > 0 && total > done ? (total - done) / speed : 0;
        $('progLabel').textContent = '正在上传 ' + Math.round(pct) + '%' +
            (batchIndex ? '（第 ' + batchIndex + '/' + batchCount + ' 批）' : '');
        $('progSub').textContent = fmtSize(done) + ' / ' + fmtSize(total) +
            (speed > 0 ? ' · ' + (speed / 1048576).toFixed(1) + ' MB/s' : '') +
            (left > 0 ? ' · 剩余约 ' + fmtDuration(left) : '');
    }

    var progressT0 = 0;

    function startUpload() {
        if (uploading || selected.length === 0) return;
        uploading = true;
        uploadAbort = false;
        autoAfter = false;
        uploadSnapshot = selected.slice();
        progressT0 = Date.now();
        renderSelected();
        $('progress').hidden = false;
        notice($('uploadNotice'), '', null);

        // 0) 先确认服务器可达
        apiJson('/info').then(function (j) {
            if (!j || j.success === false) throw new Error('unreachable');
            return runUpload().then(finishUpload);
        }).catch(function (err) {
            if (err && err.message === 'unauthorized') return;
            uploading = false;
            $('progress').hidden = true;
            renderSelected();
            notice($('uploadNotice'),
                '无法连接服务器：请确认电脑上正在运行 TransData，且设备在同一网络。', 'err');
        });
    }

    function runUpload() {
        // 1) 先核对一次，已完好的不重传（断点续传）
        var skip = {};
        var snapshot = uploadSnapshot;
        return verifyManifest(manifest(snapshot)).then(function (j) {
            (j.results || []).forEach(function (r) { if (r.status === 'ok') skip[r.path] = true; });
            var todo = snapshot.filter(function (it) { return !skip[it.path]; });
            var skipped = snapshot.length - todo.length;
            if (skipped > 0) {
                notice($('uploadNotice'), '已跳过 ' + skipped + ' 个完整文件，剩余 ' + todo.length + ' 个待传。', 'ok');
            }
            if (!todo.length) { return { done: true, failed: false, todo: [], all: snapshot }; }

            // 2) 分批上传，单批失败重试 3 次
            var batches = L.buildBatches(todo, BATCH_MAX_FILES, BATCH_MAX_BYTES);
            var total = todo.reduce(function (n, it) { return n + it.file.size; }, 0);
            var doneBytes = 0;

            var chain = Promise.resolve();
            batches.forEach(function (batch, bi) {
                chain = chain.then(function () {
                    if (uploadAbort) return;
                    var batchBytes = batch.reduce(function (n, it) { return n + it.file.size; }, 0);
                    var attempt = 0;
                    function tryOnce() {
                        attempt++;
                        notice($('uploadNotice'), '正在上传第 ' + (bi + 1) + '/' + batches.length + ' 批' +
                            (attempt > 1 ? '（第 ' + attempt + ' 次尝试）' : '') + '…', 'info');
                        // 上一次可能其实已经传完了，只是响应没回来（热点抖动、
                        // 手机切后台让 XHR 报错）。重试前先问一次服务器：已经
                        // 完整的文件不再重发，否则会落成 `_1` 副本、还多记一笔。
                        var pre = attempt > 1
                            ? verifyManifest(manifest(batch))
                            : Promise.resolve({ results: [] });
                        return pre.then(function (v) {
                            var complete = {};
                            (v.results || []).forEach(function (r) {
                                if (r.status === 'ok') complete[r.path] = true;
                            });
                            var send = batch.filter(function (it) { return !complete[it.path]; });
                            if (!send.length) return { success: true };
                            return uploadBatch(send, function (loaded) {
                                setProgress(doneBytes + loaded, total, bi + 1, batches.length);
                            });
                        }).then(function (j) {
                            if (j && j.success) return true;
                            throw new Error('batch failed');
                        }).catch(function (err) {
                            if (uploadAbort) return false;
                            if (attempt < 3) {
                                notice($('uploadNotice'), '第 ' + attempt + ' 次失败，2 秒后重试…', 'warn');
                                return new Promise(function (r) { setTimeout(r, 2000); }).then(tryOnce);
                            }
                            return false;
                        });
                    }
                    return tryOnce().then(function (ok) {
                        if (!ok) throw new Error('stop');
                        doneBytes += batchBytes;
                        setProgress(doneBytes, total, bi + 1, batches.length);
                    });
                });
            });

            return chain.then(function () {
                return { done: true, failed: false, todo: todo, all: snapshot };
            }).catch(function () {
                return { done: true, failed: true, aborted: uploadAbort };
            });
        });
    }

    function finishUpload(res) {
        uploading = false;
        $('progress').hidden = true;
        $('progBar').style.width = '0%';

        if (res && res.failed) {
            autoAfter = false;
            notice($('uploadNotice'), uploadAbort
                ? '已取消（已传完的文件保留，再点一次可继续传）。'
                : '上传失败，已保留进度；点「开始上传」可自动续传。', 'err');
            renderSelected();
            return;
        }

        var totalCount = uploadSnapshot.length;
        var todo = (res && res.todo) || uploadSnapshot;
        // 这一批（含已跳过的）从列表里摘掉；传输期间新加的文件留在列表里
        var done = {};
        ((res && res.all) || uploadSnapshot).forEach(function (it) {
            done[it.path + '|' + it.file.size] = true;
        });

        // 3) 最终清单校验
        var finish = function () {
            var keep = [];
            selected.forEach(function (it) {
                if (done[it.path + '|' + it.file.size]) releaseThumb(it);
                else keep.push(it);
            });
            selected = keep;
            renderSelected();
            loadFiles();

            // 刚才传到一半又粘了图：这批完了就接着传，不用再点一次
            if (autoAfter) {
                autoAfter = false;
                if (selected.length) startUpload();
            }
        };

        if (!todo.length) {
            notice($('uploadNotice'), '所选文件服务器上已存在，无需重复上传。', 'ok');
            finish();
            return;
        }

        verifyManifest(manifest(todo)).then(function (j) {
            var bad = (j.results || []).filter(function (r) { return r.status !== 'ok'; });
            if (j.all_ok) {
                notice($('uploadNotice'), '共 ' + totalCount + ' 个文件，校验全部通过。', 'ok');
            } else {
                var names = bad.slice(0, 8).map(function (r) { return r.path + '（' + r.status + '）'; }).join('\n');
                notice($('uploadNotice'), bad.length + ' 个文件未通过校验：\n' + names +
                    (bad.length > 8 ? '\n…共 ' + bad.length + ' 个' : '') + '\n再点一次可自动续传。', 'err');
            }
            finish();
        });
    }

    /* ------------------------------------------------------------- 下载 */

    var dlList = [];
    var dlTruncated = false;
    /* 服务端签好名的打包链接：目录相对路径 -> /zip/...?e=&d=（'' 是整个目录）。
       下载链接的签名在每条文件上（f.href）。 */
    var dlZips = {};
    var dlHrefBy = {};    // 文件路径 -> 最新签名的下载链接（重取列表后刷新）

    /* 链接里的签名到期时间（?e=Unix 秒）。页面可能一开就是十几小时，链接先
       过期了；这时点它会 401，所以点的一瞬间悄悄重取一份新签名再走同一条路。 */
    function dlExpiry(href) {
        var m = /[?&]e=(\d+)/.exec(href || '');
        return m ? parseInt(m[1], 10) : 0;
    }

    /* 过期了就重取列表并在取到后执行 next，返回 true 表示「这次点击交给我」。 */
    function resignIfNeeded(href, next) {
        if (dlExpiry(href) >= Math.floor(Date.now() / 1000)) return false;
        loadFiles().then(next);
        return true;
    }
    /* 这台设备还能不能「删除 / 修改」（由 /info 的 can_write 决定） */
    var canWrite = true;
    var dlOpenDirs = {};      // 已展开的目录路径 -> true
    var dlExpanded = false;   // 顶层是否全部展开

    function initDownload() {
        $('refreshBtn').addEventListener('click', function () {
            refreshCanWrite().then(function () { loadFiles(); });
        });
        $('toggleDl').addEventListener('click', function () {
            dlExpanded = !dlExpanded;
            renderFiles();
        });
        // 打包是边压边传的：点了之后浏览器马上开始下载，浏览器自己的下载
        // 框里能看到进度。这里只补一句提示，免得以为没反应。
        $('zipAll').addEventListener('click', function (e) {
            notice($('dlNotice'), '正在打包并下载…文件越大越慢，可以先去做别的事。', 'info');
            var href = this.getAttribute('href') || '';
            if (resignIfNeeded(href, function () {
                if (dlZips['']) { location.href = dlZips['']; }
            })) {
                e.preventDefault();
            }
        });
        loadFiles();
    }

    function loadFiles() {
        return apiJson('/files').then(function (j) {
            dlList = (j && j.files) || [];
            dlZips = (j && j.zips) || {};
            dlHrefBy = {};
            dlList.forEach(function (f) { if (f.href) dlHrefBy[f.name] = f.href; });
            dlTruncated = !!(j && j.truncated);
            renderFiles();
        }).catch(function () {
            $('dlList').textContent = '';
            $('dlEmpty').hidden = false;
            $('dlEmpty').querySelector('.empty-title').textContent = '读取失败';
            $('toggleDl').hidden = true;
        });
    }

    /* ------------------------------------------------------------------
       下载：目录树
       服务器 /files 返回的是带子目录前缀的扁平列表，buildTree 把它还原成树
       （在 /logic.js 里，有 node 单测）；目录行有展开/收起 + 打包，
       文件行只有下载。
       ------------------------------------------------------------------ */

    function renderFiles() {
        var list = $('dlList');
        list.textContent = '';

        // 「打包全部」：把整个传输目录压成一个 zip（多个文件夹时也能一把全下）
        var zipAll = $('zipAll');
        if (dlList.length) {
            zipAll.hidden = false;
            zipAll.setAttribute('href', dlZips[''] || '/zip-all');
        } else {
            zipAll.hidden = true;
        }

        if (!dlList.length) {
            $('dlEmpty').hidden = false;
            list.classList.remove('is-scrollable');
            $('toggleDl').hidden = true;
            return;
        }
        $('dlEmpty').hidden = true;

        var tree = L.buildTree(dlList, dlZips);

        // 「折叠 8 条」只作用在顶层：展开的子目录内容全部显示
        var rows = [];
        L.collectRows(tree, 0, rows, dlExpanded ? null : DL_COLLAPSED_ROWS,
            function (p) { return !!dlOpenDirs[p]; });

        var total = tree.children.length;
        var cap = Math.min(rows.length, RENDER_CAP);

        for (var i = 0; i < cap; i++) {
            list.appendChild(treeRow(rows[i].node, rows[i].depth));
        }

        // 截断必须说出来：以前服务器只给前 10000 项、前端只渲染前 200 行，
        // 两处都是静默的，用户会以为文件丢了。
        if (rows.length > cap) {
            list.appendChild(el('li', 'listmore',
                '仅渲染前 ' + cap + ' 行（共 ' + rows.length + ' 行），可展开文件夹逐个下载'));
        }
        if (dlTruncated) {
            list.appendChild(el('li', 'listmore',
                '文件太多，列表只显示前 ' + dlList.length + ' 项；其余请用「全部打包下载」'));
        }

        list.classList.toggle('is-scrollable', rows.length > DL_COLLAPSED_ROWS);

        // 不统计、不显示数字：只有一个「展开全部 / 收起」
        var tg = $('toggleDl');
        tg.hidden = total <= DL_COLLAPSED_ROWS;
        tg.textContent = dlExpanded ? '收起' : '展开全部';
        tg.setAttribute('aria-expanded', dlExpanded ? 'true' : 'false');
    }

    function treeRow(node, depth) {
        var li = el('li');
        li.style.paddingLeft = (5 + depth * 10) + 'px';
        if (node.dir) li.classList.add('is-dir');

        if (node.dir) {
            var open = !!dlOpenDirs[node.path];
            var chev = el('button', 'fi-toggle', open ? '▾' : '▸');
            chev.type = 'button';
            chev.title = open ? '收起这个文件夹' : '展开这个文件夹';
            chev.setAttribute('aria-label', chev.title);
            chev.addEventListener('click', function () {
                if (open) { delete dlOpenDirs[node.path]; }
                else { dlOpenDirs[node.path] = true; }
                renderFiles();
            });
            li.appendChild(chev);
        } else if (depth > 0) {
            // 只有真的在某个文件夹里（二级及以下）才留箭头占位做对齐；
            // 顶层的文件本身就是一级，不留白
            li.appendChild(el('span', 'fi-toggle-space'));
        }

        var ic = el('span', 'fi-icon');
        ic.appendChild(icon(node.dir ? ICON_FOLDER : ICON_FILE));
        li.appendChild(ic);

        var main = el('div', 'fi-main');
        main.appendChild(el('div', 'fi-name', node.name));
        li.appendChild(main);
        li.appendChild(el('span', 'fi-sub', node.dir
            ? node.files + ' 项 · ' + fmtSize(node.size)
            : fmtSize(node.size)));

        var actions = el('div', 'fi-actions');
        if (node.dir) {
            // 只有目录才提供「打包」
            var zip = el('a', 'btn btn--soft btn--xs', '打包');
            zip.href = node.zip;
            zip.title = '把「' + node.name + '」整个文件夹压成 zip 下载';
            zip.addEventListener('click', function (e) {
                notice($('dlNotice'), '正在打包「' + node.name + '」…边压边传，浏览器里能看到进度。', 'info');
                if (resignIfNeeded(zip.href, function () {
                    if (dlZips[node.path]) { location.href = dlZips[node.path]; }
                })) {
                    e.preventDefault();
                }
            });
            actions.appendChild(zip);
        } else {
            var dl = el('a', 'btn btn--soft btn--xs', '下载');
            dl.href = node.href;
            dl.addEventListener('click', function (e) {
                if (resignIfNeeded(dl.href, function () {
                    if (dlHrefBy[node.path]) { location.href = dlHrefBy[node.path]; }
                })) {
                    e.preventDefault();
                }
            });
            actions.appendChild(dl);
        }
        actions.appendChild(renameButton(node));
        actions.appendChild(deleteButton(node.path, node.name, node.dir));
        li.appendChild(actions);
        return li;
    }

    /* 重命名 / 移动：同一个接口（服务端 rename 是原子的，见 /rename） */
    function renameButton(node) {
        var btn = el('button', 'icon-btn icon-btn--ok');
        btn.type = 'button';
        btn.title = '重命名 / 移动';
        btn.setAttribute('aria-label', '重命名 ' + node.name);
        btn.appendChild(icon(ICON_PENCIL));
        if (!canWrite) {
            btn.disabled = true;
            btn.title = '电脑端已禁止这台设备删除 / 修改';
            return btn;
        }
        btn.addEventListener('click', function () {
            openDialog({
                title: '重命名 / 移动',
                text: '可以带子目录，例如 归档/' + node.name,
                input: node.path,
                okLabel: '改名'
            }).then(function (next) {
                if (next == null) return;
                next = next.replace(/^\/+|\/+$/g, '');
                if (!next) return;
                if (next === node.path) {
                    notice($('dlNotice'), '名字没有变化', 'info');
                    return;
                }
                apiJson('/rename', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({ from: node.path, to: next })
                }).then(function (j) {
                    if (j && j.success) {
                        notice($('dlNotice'), '已改名：' + node.name + ' → ' + next, 'ok');
                        loadFiles();
                    } else {
                        notice($('dlNotice'), '重命名失败：' + ((j && j.error) || '未知错误'), 'err');
                    }
                }).catch(function () {
                    notice($('dlNotice'), '重命名失败（网络错误）', 'err');
                });
            });
        });
        return btn;
    }

    function deleteButton(path, name, isDir) {
        var btn = el('button', 'icon-btn icon-btn--ok');
        btn.type = 'button';
        btn.title = isDir ? '删除整个文件夹' : '删除';
        btn.setAttribute('aria-label', (isDir ? '删除文件夹 ' : '删除 ') + name);
        btn.appendChild(icon(ICON_TRASH));
        if (!canWrite) {
            btn.disabled = true;
            btn.title = '电脑端已禁止这台设备删除 / 修改';
            return btn;
        }
        btn.addEventListener('click', function () {
            var what = isDir
                ? '整个文件夹「' + name + '」及其里的所有文件'
                : '「' + name + '」';
            openDialog({
                title: '删除' + (isDir ? '文件夹' : '文件'),
                text: '删除' + what + '？删除后不可恢复。',
                okLabel: '删除',
                danger: true
            }).then(function (ok) {
                if (!ok) return;
                apiJson('/delete/' + encPath(path), { method: 'DELETE' }).then(function (j) {
                    if (j && j.success) {
                        notice($('dlNotice'), '已删除 ' + name, 'ok');
                        loadFiles();
                    } else {
                        notice($('dlNotice'), '删除失败：' + ((j && j.error) || '未知错误'), 'err');
                    }
                }).catch(function () {
                    notice($('dlNotice'), '删除失败（网络错误）', 'err');
                });
            });
        });
        return btn;
    }

    function encPath(p) {
        return L.encPath(p);
    }

    /* ------------------------------------------------------------- 便签 */

    var noteLastV = 0;
    var noteWs = null;
    var noteSyncedAt = 0;

    /* 输入框随内容长高（3~6 行），矮输入框在小屏上不好用 */
    function autoGrowNote() {
        var text = $('noteText');
        if (!text) return;
        text.style.height = 'auto';
        text.style.height = Math.max(62, Math.min(text.scrollHeight, 140)) + 'px';
    }

    function initNote() {
        var text = $('noteText');
        var status = $('noteStatus');

        text.addEventListener('input', function () {
            updateNoteMeta();
            autoGrowNote();
        });

        $('noteSend').addEventListener('click', function () {
            var body = text.value;
            apiJson('/note', {
                method: 'POST',
                headers: { 'Content-Type': 'text/plain; charset=utf-8' },
                body: body
            }).then(function (j) {
                if (j && j.success) {
                    noteLastV = j.v;
                    noteSyncedAt = Date.now();
                    updateNoteMeta();
                    status.textContent = '已发送，另一端会自动填入';
                } else {
                    status.textContent = '发送失败';
                }
            }).catch(function () { status.textContent = '发送失败（网络错误）'; });
        });

        $('noteCopy').addEventListener('click', function () {
            apiJson('/note').then(function (j) {
                if (j && j.success) {
                    text.value = j.text || '';
                    noteLastV = j.v || 0;
                    noteSyncedAt = Date.now();
                    updateNoteMeta();
                    autoGrowNote();
                }
            }).catch(function () { /* 保留当前内容 */ })
                .then(function () {
                    text.focus();
                    text.select();
                    return copyText(text.value);
                }).then(function (ok) {
                    status.textContent = ok ? '已复制到剪贴板' : '已全选，长按文字即可复制';
                });
        });

        apiJson('/note').then(function (j) {
            if (j && j.success) {
                text.value = j.text || '';
                noteLastV = j.v || 0;
                noteSyncedAt = Date.now();
                updateNoteMeta();
                autoGrowNote();
            }
        }).catch(function () {});

        connectNoteWs();
    }

    function updateNoteMeta() {
        var n = $('noteText').value.length;
        var when = noteSyncedAt ? ' · ' + fmtTime(noteSyncedAt) : '';
        $('noteMeta').textContent = n + ' 字' + when;
    }

    function connectNoteWs() {
        if (noteWs && (noteWs.readyState === 0 || noteWs.readyState === 1)) return;
        var proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
        try {
            noteWs = new WebSocket(proto + '//' + location.host + '/ws');
        } catch (e) {
            noteWs = null;
            setTimeout(connectNoteWs, 3000);
            return;
        }
        noteWs.onmessage = function (e) {
            var m;
            try { m = JSON.parse(e.data); } catch (err) { return; }
            if (!m || typeof m.v !== 'number' || m.v <= noteLastV) return;
            noteLastV = m.v;
            $('noteText').value = m.text || '';
            noteSyncedAt = Date.now();
            updateNoteMeta();
            autoGrowNote();
            $('noteStatus').textContent = '已收到对方内容';
        };
        noteWs.onclose = function () { noteWs = null; setTimeout(connectNoteWs, 2500); };
        noteWs.onerror = function () { try { noteWs.close(); } catch (e) { } };
    }

    /* --------------------------------------------------------------- 启动 */

    if (MODE_APP) initApp(); else initPair();
})();

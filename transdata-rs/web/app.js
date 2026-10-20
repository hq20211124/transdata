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

    /* ---------------------------------------------------------------- utils */

    function fmtSize(bytes) {
        if (!bytes && bytes !== 0) return '';
        if (bytes < 1024) return bytes + ' B';
        var units = ['KB', 'MB', 'GB', 'TB'];
        var v = bytes / 1024, i = 0;
        while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
        return (v < 10 ? v.toFixed(1) : Math.round(v)) + ' ' + units[i];
    }

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
        initDownload();
        initNote();
        initServerInfo();
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
            setConn(true, '已连接');
        }).catch(function () {
            setConn(false, '连接断开');
        });
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

    var selected = [];      // [{file, path}]
    var uploading = false;
    var uploadXhr = null;
    var uploadAbort = false;
    var BATCH_MAX_FILES = 32;
    var BATCH_MAX_BYTES = 256 * 1024 * 1024;
    var RENDER_CAP = 200;
    /* 折叠状态下只显示前几行，展开后列表自己滚动 —— 页面永远不会被撑长 */
    var UPLOAD_COLLAPSED_ROWS = 5;
    var DL_COLLAPSED_ROWS = 5;
    var filesExpanded = false;
    var dlExpanded = false;

    function initUpload() {
        var dz = $('dropzone');
        var fileInput = $('fileInput');
        var folderInput = $('folderInput');

        $('pickFiles').addEventListener('click', function (e) { e.stopPropagation(); fileInput.click(); });
        $('pickFolder').addEventListener('click', function (e) { e.stopPropagation(); folderInput.click(); });

        dz.addEventListener('click', function () { fileInput.click(); });
        dz.addEventListener('keydown', function (e) {
            if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); fileInput.click(); }
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
        var seen = {};
        for (var i = 0; i < selected.length; i++) {
            seen[selected[i].path + '|' + selected[i].file.size] = true;
        }
        for (var j = 0; j < picked.length; j++) {
            var it = picked[j];
            var k = it.path + '|' + it.file.size;
            if (!seen[k]) { selected.push(it); seen[k] = true; }
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

            var ic = el('span', 'fi-icon');
            ic.appendChild(icon(ICON_FILE));
            li.appendChild(ic);

            var main = el('div', 'fi-main');
            main.appendChild(el('div', 'fi-name', it.path));
            main.appendChild(el('div', 'fi-sub', fmtSize(it.file.size)));
            li.appendChild(main);

            var actions = el('div', 'fi-actions');
            var rm = el('button', 'icon-btn');
            rm.type = 'button';
            rm.title = '移除';
            rm.setAttribute('aria-label', '移除 ' + it.path);
            rm.textContent = '×';
            rm.addEventListener('click', function () {
                selected.splice(idx, 1);
                renderSelected();
            });
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

    function buildBatches(items) {
        var batches = [], cur = [], curBytes = 0;
        for (var i = 0; i < items.length; i++) {
            var it = items[i];
            if (cur.length > 0 && (cur.length >= BATCH_MAX_FILES || curBytes + it.file.size > BATCH_MAX_BYTES)) {
                batches.push(cur); cur = []; curBytes = 0;
            }
            cur.push(it); curBytes += it.file.size;
        }
        if (cur.length) batches.push(cur);
        return batches;
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
        return verifyManifest(manifest(selected)).then(function (j) {
            (j.results || []).forEach(function (r) { if (r.status === 'ok') skip[r.path] = true; });
            var todo = selected.filter(function (it) { return !skip[it.path]; });
            var skipped = selected.length - todo.length;
            if (skipped > 0) {
                notice($('uploadNotice'), '已跳过 ' + skipped + ' 个完整文件，剩余 ' + todo.length + ' 个待传。', 'ok');
            }
            if (!todo.length) { return { done: true, failed: false, todo: [] }; }

            // 2) 分批上传，单批失败重试 3 次
            var batches = buildBatches(todo);
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
                        return uploadBatch(batch, function (loaded) {
                            setProgress(doneBytes + loaded, total, bi + 1, batches.length);
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
                return { done: true, failed: false, todo: todo };
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
            notice($('uploadNotice'), uploadAbort
                ? '已取消（已传完的文件保留，再点一次可继续传）。'
                : '上传失败，已保留进度；点「开始上传」可自动续传。', 'err');
            renderSelected();
            return;
        }

        var totalCount = selected.length;
        var todo = (res && res.todo) || selected;

        // 3) 最终清单校验
        var finish = function () {
            selected = [];
            $('fileList').textContent = '';
            renderSelected();
            loadFiles();
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
    var dlOpenDirs = {};      // 已展开的目录路径 -> true
    var dlExpanded = false;   // 顶层是否全部展开

    function initDownload() {
        $('refreshBtn').addEventListener('click', function () { loadFiles(); });
        $('toggleDl').addEventListener('click', function () {
            dlExpanded = !dlExpanded;
            renderFiles();
        });
        loadFiles();
    }

    function loadFiles() {
        return apiJson('/files').then(function (j) {
            dlList = (j && j.files) || [];
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
       服务器 /files 返回的是带子目录前缀的扁平列表，这里还原成树：
       目录行有展开/收起 + 打包（整个目录压成 zip），文件行只有下载。
       ------------------------------------------------------------------ */

    function buildTree(files) {
        var root = { name: '', path: '', dir: true, children: [], files: 0, size: 0, parent: null };
        var index = { '': root };
        files.forEach(function (f) {
            var parts = f.name.split('/');
            var cur = root;
            for (var i = 0; i < parts.length - 1; i++) {
                var p = parts.slice(0, i + 1).join('/');
                var node = index[p];
                if (!node) {
                    node = {
                        name: parts[i], path: p, dir: true, children: [],
                        files: 0, size: 0, parent: cur
                    };
                    index[p] = node;
                    cur.children.push(node);
                }
                cur = node;
            }
            cur.children.push({
                name: parts[parts.length - 1], path: f.name, dir: false,
                children: [], files: 1, size: f.size, parent: cur
            });
            // 目录上的计数/大小向上累加
            var up = cur;
            while (up) { up.files++; up.size += f.size; up = up.parent; }
        });
        sortTree(root);
        return root;
    }

    /* 目录在前、文件在后，同类按名字排（跟文件管理器一个顺序） */
    function sortTree(node) {
        node.children.sort(function (a, b) {
            if (a.dir !== b.dir) return a.dir ? -1 : 1;
            return a.name.localeCompare(b.name, 'zh-Hans-CN');
        });
        node.children.forEach(function (c) {
            if (c.dir) sortTree(c);
        });
    }

    function renderFiles() {
        var list = $('dlList');
        list.textContent = '';

        // 「打包全部」：把整个传输目录压成一个 zip（多个文件夹时也能一把全下）
        var zipAll = $('zipAll');
        if (dlList.length) {
            zipAll.hidden = false;
            zipAll.setAttribute('href', '/zip-all');
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

        var tree = buildTree(dlList);

        // 「折叠 5 条」只作用在顶层：展开的子目录内容全部显示
        var rows = [];
        collectRows(tree, 0, rows, dlExpanded ? null : DL_COLLAPSED_ROWS);

        var total = tree.children.length;
        var rootShown = dlExpanded ? total : Math.min(total, DL_COLLAPSED_ROWS);
        var cap = Math.min(rows.length, RENDER_CAP);

        for (var i = 0; i < cap; i++) {
            list.appendChild(treeRow(rows[i].node, rows[i].depth));
        }

        list.classList.toggle('is-scrollable', rows.length > DL_COLLAPSED_ROWS);

        // 不统计、不显示数字：只有一个「展开全部 / 收起」
        var tg = $('toggleDl');
        tg.hidden = total <= DL_COLLAPSED_ROWS;
        tg.textContent = dlExpanded ? '收起' : '展开全部';
        tg.setAttribute('aria-expanded', dlExpanded ? 'true' : 'false');
    }

    /* 把树摊平成 [{node, depth}]，只展开用户点开过的目录。
       limit 只作用在本层（顶层折叠用），子目录一旦展开就全部显示。 */
    function collectRows(node, depth, out, limit) {
        var n = (limit == null) ? node.children.length : Math.min(node.children.length, limit);
        for (var i = 0; i < n; i++) {
            var child = node.children[i];
            out.push({ node: child, depth: depth });
            if (child.dir && dlOpenDirs[child.path]) {
                collectRows(child, depth + 1, out, null);
            }
        }
    }

    function treeRow(node, depth) {
        var li = el('li');
        li.style.paddingLeft = (10 + depth * 14) + 'px';
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
        } else {
            // 文件行留一个同样宽的占位，名字和目录行对齐
            li.appendChild(el('span', 'fi-toggle-space'));
        }

        var ic = el('span', 'fi-icon');
        ic.appendChild(icon(node.dir ? ICON_FOLDER : ICON_FILE));
        li.appendChild(ic);

        var main = el('div', 'fi-main');
        if (node.dir) main.classList.add('fi-col');   // 目录：名字独占一行，别被副标题挤没
        main.appendChild(el('div', 'fi-name', node.name));
        main.appendChild(el('div', 'fi-sub', node.dir
            ? '文件夹 · ' + node.files + ' 个文件 · ' + fmtSize(node.size)
            : fmtSize(node.size)));
        li.appendChild(main);

        var actions = el('div', 'fi-actions');
        if (node.dir) {
            // 只有目录才提供「打包」
            var zip = el('a', 'btn btn--soft btn--sm', '打包');
            zip.href = '/zip/' + encPath(node.path);
            zip.title = '把「' + node.name + '」整个文件夹压成 zip 下载';
            actions.appendChild(zip);
        } else {
            var dl = el('a', 'btn btn--soft btn--sm', '下载');
            dl.href = '/download/' + encPath(node.path);
            actions.appendChild(dl);
        }
        actions.appendChild(deleteButton(node.path, node.name, node.dir));
        li.appendChild(actions);
        return li;
    }

    function deleteButton(path, name, isDir) {
        var btn = el('button', 'icon-btn icon-btn--ok');
        btn.type = 'button';
        btn.title = isDir ? '删除整个文件夹' : '删除';
        btn.setAttribute('aria-label', (isDir ? '删除文件夹 ' : '删除 ') + name);
        btn.appendChild(icon(ICON_TRASH));
        btn.addEventListener('click', function () {
            var what = isDir
                ? '整个文件夹「' + name + '」及其里的所有文件'
                : '「' + name + '」';
            if (!confirm('删除' + what + '？该操作不可恢复。')) return;
            apiJson('/delete/' + encPath(path), { method: 'DELETE' }).then(function (j) {
                if (j && j.success) { loadFiles(); }
                else { alert('删除失败：' + ((j && j.error) || '未知错误')); }
            }).catch(function () { alert('删除失败'); });
        });
        return btn;
    }

    function encPath(p) {
        return p.split('/').map(encodeURIComponent).join('/');
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

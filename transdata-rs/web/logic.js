/* ==========================================================================
   TransData · 纯逻辑层（不碰 DOM，可在 node 里直接 require）

   为什么单独拆一个文件：上传列表的「×」按钮曾经因为 `var` 闭包捕获了循环
   变量而删错行/点了没反应 —— 这种 bug 只有把纯逻辑拿出来跑测试才拦得住。
   浏览器里这个文件由 src/web.rs 在编译期拼在 app.js 前面，挂到 window.TDLogic，
   所以对外仍然只有一个 /app.js 请求，成品体积不变。
   ========================================================================== */
(function (root, factory) {
    if (typeof module === 'object' && module.exports) {
        module.exports = factory();
    } else {
        root.TDLogic = factory();
    }
})(typeof self !== 'undefined' ? self : this, function () {
    'use strict';

    /* 字节数 → 人类可读 */
    function fmtSize(bytes) {
        if (!bytes && bytes !== 0) return '';
        if (bytes < 1024) return bytes + ' B';
        var units = ['KB', 'MB', 'GB', 'TB'];
        var v = bytes / 1024, i = 0;
        while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
        return (v < 10 ? v.toFixed(1) : Math.round(v)) + ' ' + units[i];
    }

    /* 分批：每批 ≤ maxFiles 个且 ≤ maxBytes 字节（单个超大文件自成一批） */
    function buildBatches(items, maxFiles, maxBytes) {
        var batches = [], cur = [], curBytes = 0;
        for (var i = 0; i < items.length; i++) {
            var it = items[i];
            if (cur.length > 0 && (cur.length >= maxFiles || curBytes + it.file.size > maxBytes)) {
                batches.push(cur); cur = []; curBytes = 0;
            }
            cur.push(it); curBytes += it.file.size;
        }
        if (cur.length) batches.push(cur);
        return batches;
    }

    /* 合并新选中的文件：同一「路径 + 大小」只保留一份。返回新数组，不改入参 */
    function mergePicked(selected, picked) {
        var out = selected.slice();
        var seen = {};
        for (var i = 0; i < out.length; i++) {
            seen[out[i].path + '|' + out[i].file.size] = true;
        }
        for (var j = 0; j < picked.length; j++) {
            var it = picked[j];
            var k = it.path + '|' + it.file.size;
            if (!seen[k]) { out.push(it); seen[k] = true; }
        }
        return out;
    }

    /* 按对象引用删一项，返回新数组。
       绝不能用下标 —— 列表是折叠渲染的，显示的行号和 selected 的下标不是一回事。 */
    function removeItem(list, item) {
        var i = list.indexOf(item);
        if (i < 0) return list.slice();
        var out = list.slice();
        out.splice(i, 1);
        return out;
    }

    /* 把 /files 的扁平列表（name 带子目录前缀）还原成树 */
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
            var up = cur;
            while (up) { up.files++; up.size += f.size; up = up.parent; }
        });
        sortTree(root);
        return root;
    }

    /* 目录在前、文件在后，同类按名字排 */
    function sortTree(node) {
        node.children.sort(function (a, b) {
            if (a.dir !== b.dir) return a.dir ? -1 : 1;
            return a.name.localeCompare(b.name, 'zh-Hans-CN');
        });
        node.children.forEach(function (c) {
            if (c.dir) sortTree(c);
        });
    }

    /* 树 → [{node, depth}]，只展开 isOpen(path) 为真的目录。
       limit 只作用在本层（顶层折叠），子目录一旦展开就全显示。 */
    function collectRows(node, depth, out, limit, isOpen) {
        var n = (limit == null) ? node.children.length : Math.min(node.children.length, limit);
        for (var i = 0; i < n; i++) {
            var child = node.children[i];
            out.push({ node: child, depth: depth });
            if (child.dir && isOpen(child.path)) {
                collectRows(child, depth + 1, out, null, isOpen);
            }
        }
        return out;
    }

    /* 路径 → URL：逐段编码（`/` 保留为分隔符） */
    function encPath(p) {
        return p.split('/').map(encodeURIComponent).join('/');
    }

    /* 截图文件名：截图_20261031_142530.png；同一秒多张用 _2、_3 区分 */
    function shotName(date, seq, ext) {
        var p = function (n) { return (n < 10 ? '0' : '') + n; };
        var stamp = '' + date.getFullYear() + p(date.getMonth() + 1) + p(date.getDate()) +
            '_' + p(date.getHours()) + p(date.getMinutes()) + p(date.getSeconds());
        return '截图_' + stamp + (seq > 1 ? '_' + seq : '') + '.' + ext;
    }

    return {
        fmtSize: fmtSize,
        buildBatches: buildBatches,
        mergePicked: mergePicked,
        removeItem: removeItem,
        buildTree: buildTree,
        sortTree: sortTree,
        collectRows: collectRows,
        encPath: encPath,
        shotName: shotName
    };
});

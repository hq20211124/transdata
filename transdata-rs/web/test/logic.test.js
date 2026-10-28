/* ==========================================================================
   TransData · 前端纯逻辑单测（零依赖，node 内置 test runner）

   跑法：node --test transdata-rs/web/test/
   为什么要有这层：上传列表的「×」曾经删错行（闭包捕获了循环变量），
   这类 bug 在浏览器里靠手点是很难稳定复现的，必须靠纯函数测试拦住。
   ========================================================================== */
'use strict';

const test = require('node:test');
const assert = require('node:assert');
const L = require('../logic.js');

/* 造一个「已选中」条目，形状与 app.js 里的 {file, path} 一致 */
function item(path, size) {
    return { file: { name: path.split('/').pop(), size: size }, path: path };
}

test('removeItem 按对象引用删，不受显示行号影响', function () {
    // 12 个文件、列表只显示前 8 行 —— 点第 9 个（下标 8）必须删掉第 9 个，
    // 而不是「第 8 行对应的那个」。旧实现用循环变量做下标，这里会挂。
    const list = [];
    for (let i = 0; i < 12; i++) list.push(item('f' + i + '.txt', 10));
    const target = list[8];

    const out = L.removeItem(list, target);

    assert.strictEqual(out.length, 11);
    assert.strictEqual(out.indexOf(target), -1, '被点的那个必须消失');
    assert.strictEqual(out[8].path, 'f9.txt', '后面的元素整体前移，顺序不变');
    assert.strictEqual(list.length, 12, '入参不能被改');
});

test('removeItem 点最后一个也能删掉（旧实现这里是空操作）', function () {
    const list = [item('a', 1), item('b', 2), item('c', 3)];
    const out = L.removeItem(list, list[2]);
    assert.deepStrictEqual(out.map((x) => x.path), ['a', 'b']);
});

test('removeItem 对不在列表里的对象是空操作', function () {
    const list = [item('a', 1)];
    const out = L.removeItem(list, item('zzz', 9));
    assert.strictEqual(out.length, 1);
});

test('mergePicked 按 path+size 去重，同名不同大小算两个', function () {
    const a = item('x/1.txt', 10);
    const sameAgain = item('x/1.txt', 10);
    const bigger = item('x/1.txt', 20);

    const out = L.mergePicked([a], [sameAgain, bigger, item('y.txt', 1)]);

    assert.strictEqual(out.length, 3, '重复的那份被丢掉，其余保留');
    assert.strictEqual(out[0], a);
});

test('buildBatches 按文件数和字节数切批', function () {
    const many = [];
    for (let i = 0; i < 33; i++) many.push(item('f' + i, 1));
    const b = L.buildBatches(many, 32, 256);
    assert.strictEqual(b.length, 2);
    assert.strictEqual(b[0].length, 32);
    assert.strictEqual(b[1].length, 1);

    // 字节上限先到
    const heavy = [item('a', 200), item('b', 200), item('c', 10)];
    const b2 = L.buildBatches(heavy, 32, 256);
    assert.deepStrictEqual(b2.map((x) => x.length), [1, 2]);
});

test('buildBatches 单个超大文件自成一批（不能被无限切分）', function () {
    const b = L.buildBatches([item('huge', 10 * 1024 * 1024 * 1024)], 32, 256);
    assert.strictEqual(b.length, 1);
    assert.strictEqual(b[0].length, 1);
});

test('buildTree 还原目录树并向上累加数量/大小', function () {
    const tree = L.buildTree([
        { name: 'a/1.txt', size: 10, modified: 0 },
        { name: 'a/2.txt', size: 20, modified: 0 },
        { name: 'a/sub/3.txt', size: 30, modified: 0 },
        { name: 'top.txt', size: 5, modified: 0 }
    ]);

    const a = tree.children.find((c) => c.name === 'a');
    assert.ok(a && a.dir, 'a 应该是目录');
    assert.strictEqual(a.files, 3);
    assert.strictEqual(a.size, 60);
    assert.strictEqual(tree.files, 4, '根节点也要累加');
    assert.strictEqual(tree.size, 65);

    const sub = a.children.find((c) => c.name === 'sub');
    assert.strictEqual(sub.files, 1);
    assert.strictEqual(sub.path, 'a/sub', 'path 是相对路径，用于 /zip/ 与展开状态');
});

test('buildTree 目录排在文件前面', function () {
    const tree = L.buildTree([
        { name: 'zzz.txt', size: 1, modified: 0 },
        { name: 'aaa/b.txt', size: 1, modified: 0 }
    ]);
    assert.strictEqual(tree.children[0].name, 'aaa');
    assert.strictEqual(tree.children[0].dir, true);
    assert.strictEqual(tree.children[1].name, 'zzz.txt');
});

test('collectRows 只展开打开过的目录，limit 只作用在本层', function () {
    const tree = L.buildTree([
        { name: 'd1/a.txt', size: 1, modified: 0 },
        { name: 'd1/b.txt', size: 1, modified: 0 },
        { name: 'd2/c.txt', size: 1, modified: 0 }
    ]);

    const closed = L.collectRows(tree, 0, [], 8, () => false);
    assert.strictEqual(closed.length, 2, '折叠时只有两个顶层目录');

    const opened = L.collectRows(tree, 0, [], 8, (p) => p === 'd1');
    assert.deepStrictEqual(opened.map((r) => r.node.path),
        ['d1', 'd1/a.txt', 'd1/b.txt', 'd2']);
    assert.strictEqual(opened[1].depth, 1);

    const limited = L.collectRows(tree, 0, [], 1, () => false);
    assert.strictEqual(limited.length, 1, 'limit 只截本层');
});

test('encPath 逐段编码，斜杠保留为分隔符', function () {
    assert.strictEqual(L.encPath('a b/c.txt'), 'a%20b/c.txt');
    assert.strictEqual(L.encPath('中文/50%41.txt'), '%E4%B8%AD%E6%96%87/50%2541.txt');
    assert.strictEqual(L.encPath('a+b.txt'), 'a%2Bb.txt');
});

test('fmtSize 边界', function () {
    assert.strictEqual(L.fmtSize(0), '0 B');
    assert.strictEqual(L.fmtSize(1023), '1023 B');
    assert.strictEqual(L.fmtSize(1024), '1.0 KB');
    assert.strictEqual(L.fmtSize(1024 * 1024 * 1024 * 3), '3.0 GB');
    assert.strictEqual(L.fmtSize(null), '');
});

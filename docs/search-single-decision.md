# rsou 搜索决策单一化改造

> **状态**：本任务已在分支 `refactor/fts-single-decision` 落地，提交 `7ffc5cb`
> 「检索判定单一化:FTS MATCH 成为唯一命中来源」。本文保留为完整任务书与边界说明；
> 第 26 节列出的问题在本次改造后仍然存在，留待后续独立任务处理。

## 1. 任务目标

对 rsou 当前全文检索链路进行一次**最小化架构修正**。

本次唯一核心目标：

> **SQLite FTS5 `MATCH` 成为“文档是否命中”的唯一判定来源。**

当前搜索链路存在两套判定逻辑：

```text
query.rs
   ↓
FTS5 MATCH
   ↓
候选文档
   ↓
locate_literals()
   ↓
应用层再次判断文档是否真正命中
   ↓
最终 SearchResponse
```

改造后：

```text
query.rs
   ↓
FTS5 MATCH
   ↓
最终命中文档
   ↓
Display Locator
   ├─ 标题高亮
   ├─ 正文高亮
   ├─ Hit
   ├─ chunk 映射
   └─ 预览导航
```

关键原则：

```text
FTS5 MATCH = true
```

则该文档必须保留在最终搜索结果中。

任何展示层定位结果都不得再次否决 FTS 结果。

---

## 2. 本任务明确不做的事情

本次必须控制范围。

不要顺带进行以下改造：

* 不重做 tokenizer；
* 不升级 tokenizer v2；
* 不改变中文逐字 tokenizer 当前语义；
* 不改变 ASCII token 当前语义；
* 不改变标点处理规则；
* 不删除 jieba；
* 不删除精确/宽松模式；
* 不重写整个 query parser；
* 不引入新的 Plan evaluator；
* 不实现 Candidate::AllDocuments；
* 不改 SQLite schema；
* 不改 contentless-delete；
* 不改 documents / document_contents / chunks；
* 不改 BM25 权重；
* 不改结果卡 UI；
* 不增加 snippet；
* 不改右侧全文预览模式；
* 不增加分页；
* 不在本任务解决 `total_hits` 截断问题；
* 不在本任务解决跨空白高亮只定位第一处问题；
* 不在本任务解决 NOT / 字段 / jieba 的其他独立 parser/compiler bug。

这些问题统一留待后续独立任务处理。

---

## 3. 当前核心问题

当前 `src/search.rs` 在 FTS 已经返回候选文档之后，还执行：

```rust
let content_highlights = locate_literals(content, literals);
let title_highlights = locate_literals(&document.title, literals);

if content_highlights.is_empty() && title_highlights.is_empty() {
    continue;
}
```

这意味着：

```text
FTS5：
命中
```

并不等于最终：

```text
rsou：
命中
```

应用层的 `locate_literals()` 仍拥有第二次否决权。

这造成两个问题：

1. FTS tokenizer 与字符串定位器语义不一致时，必须决定“谁才是正确搜索语义”；
2. 高亮/定位层的 bug 会直接改变搜索结果集合。

本次改造后必须消除这一点。

---

## 4. 核心不变式

### 4.1 FTS 是唯一真假判定

```text
documents_fts MATCH ?
```

返回的文档，在满足结构化 SQL filter 后，即属于真实搜索结果。

应用层不得再次判断：

```text
是否满足 literal
是否存在高亮
是否存在 Hit
是否存在 fragment
```

### 4.2 Locator 只负责展示

以下对象：

```text
locate_literals()
title_highlights
content_highlights
group_into_fragments()
Hit
chunks
context_header
PreviewSpanCache
```

全部属于：

```text
Display / Navigation
```

而不是：

```text
Search Truth
```

### 4.3 定位失败不得过滤搜索结果

以下情况必须允许：

```text
FTS5：命中
Locator：未找到任何 span
```

最终仍然返回该文档。

允许：

```rust
title_highlights = Vec::new();
hits = Vec::new();
```

但不允许：

```rust
continue;
```

---

## 5. `src/search.rs` 核心修改

定位当前类似代码：

```rust
let content_highlights = locate_literals(content, literals);
let title_highlights = locate_literals(&document.title, literals);

if content_highlights.is_empty() && title_highlights.is_empty() {
    continue;
}

let bounds = chunk_ranges(conn, id).unwrap_or_default();
let hits = group_into_fragments(...);
```

删除二次判定：

```rust
let content_highlights = locate_literals(content, literals);
let title_highlights = locate_literals(&document.title, literals);

// FTS MATCH 已经完成最终搜索判定。
// highlights/hits 只用于 UI 展示与导航，不得再否决结果。

let bounds = chunk_ranges(conn, id).unwrap_or_default();
let hits = group_into_fragments(...);
```

不得加入等价形式，例如：

```rust
if hits.is_empty() {
    continue;
}
```

或者：

```rust
if no_display_evidence {
    continue;
}
```

任何展示层状态均不得过滤 FTS 结果。

---

## 6. 修改函数和注释语义

当前涉及：

```text
locate_literals
find_ignoring_whitespace
group_into_fragments
search
```

的注释中，如果出现：

```text
精确复核
二次过滤
验证 FTS 候选
排除假阳性
```

全部改写。

推荐统一描述：

```rust
/// 在原文中定位正向查询项，用于 UI 高亮和命中导航。
///
/// 本函数不参与搜索真假判定；即使返回空结果，也不得移除 FTS5 已命中的文档。
```

---

## 7. `search()` 主流程注释同步修改

当前如果存在：

```text
编译查询
→ FTS候选
→ 精确复核
→ 片段
```

改成：

```text
编译查询
→ FTS最终检索
→ 结构化过滤与排名
→ 为最终结果生成展示高亮与片段
```

必须明确：

> FTS MATCH 是全文文本条件唯一的搜索判定来源。

---

## 8. “候选”术语最小清理

如果代码中只是内部变量：

```text
candidate_ids
candidate_rows
```

且实际现在已经属于 FTS 最终命中结果，优先改为：

```text
matched_ids
matched_rows
```

如果改名会造成大范围无意义 diff，可以暂时保留变量名，但注释不得再表达：

> 后续还有第二套搜索逻辑决定真假。

---

## 9. `MAX_CANDIDATES = 200`

为了最小化改造，本次不重写排名和分页 SQL。

现有 200 上限可以暂时保留。

但语义必须从：

```text
最多取 200 个候选，再精确复核
```

改成：

```text
最多读取排名靠前的 200 个 FTS 命中内容组，
供当前 UI 后续展示处理
```

如果修改成本较低，建议：

```rust
MAX_CANDIDATES
```

重命名为：

```rust
MAX_RANKED_GROUPS
```

本任务不解决超过 200 个结果后的分页或继续加载问题。

---

## 10. SearchResponse 语义

现有：

```rust
total_documents
total_groups
```

继续使用。

注释改为：

```text
total_documents
= 满足 FTS MATCH + 结构化过滤条件的文件位置数

total_groups
= 满足 FTS MATCH + 结构化过滤条件的内容组数
```

不得再描述为：

```text
FTS 候选数量
```

或：

```text
待精确复核数量
```

---

## 11. GUI 状态文案

如果当前显示类似：

```text
FTS 候选 700 组、1000 个位置，
部分未展示或未通过精确匹配
```

必须删除：

```text
未通过精确匹配
```

可改为：

```text
共命中 700 组、1000 个文件位置，
当前仅展示排名靠前的部分结果
```

保持现有简洁风格即可。

---

## 12. Locator 空结果的 GUI 退化

FTS 命中文档但：

```rust
hits.is_empty()
```

时，不得因此移除文档。

建议：

```text
hits > 0
→ 显示“命中 N 处”

hits == 0 && title_highlights 非空
→ 显示“标题命中”

hits == 0 && title_highlights 为空
→ 不显示命中处数
```

如果本次不便修改 UI 文案，至少保证：

```text
文档仍然显示
文档仍然可以点击
右侧仍然可以加载全文
```

---

## 13. 右侧预览

保持现有：

```text
点击文档
→ start_preview()
→ plain_text
→ preview_window()
→ locate_literals()
→ span_job()
```

不改成 Hit-only。

不删除现有：

```text
PreviewMsg
preview_text
preview_anchor
preview_spans_cache
PREVIEW_MAX_BYTES
PREVIEW_WINDOW_BYTES
```

---

## 14. 预览定位失败

如果：

```rust
locate_literals(window, literals)
```

返回空：

```text
继续显示原文
```

不得：

* 清空该搜索结果；
* 重新判断文档是否命中；
* 显示“文档不再匹配”；
* 触发重新检索过滤。

Locator 只影响高亮和导航。

---

## 15. 精确模式与宽松模式

本任务全部保留。

### 精确模式

继续：

```text
用户输入
→ query.rs
→ FTS phrase / MATCH
```

### 宽松模式

继续：

```text
用户输入
→ jieba
→ 查询改写
→ FTS MATCH
```

jieba 属于：

```text
Query Rewrite
```

不属于：

```text
Search Decision
```

因此与搜索决策单一化并不冲突。

---

## 16. 同义词

同义词逻辑继续保留。

例如：

```text
电脑
→ 电脑 OR 计算机 OR PC
```

最终仍然统一编译成 FTS MATCH。

本任务不要修改现有同义词策略。

---

## 17. Query Parser / AST

本任务不要求重新设计 AST。

继续沿用当前结构，例如：

```text
Term
Phrase
And
Or
Not
Field
```

只需要确保：

```text
query.rs
↓
FTS MATCH
```

后，不再存在第二套应用层布尔执行器。

---

## 18. 不实现 Plan evaluator

不得为了修复当前二次复核而新增：

```rust
enum Plan {
    Leaf,
    All,
    Any,
    Not,
}
```

并在读取 `title/plain_text` 后调用：

```rust
scan_eval(...)
```

重新判断真假。

本任务明确禁止：

```text
FTS MATCH
+
应用层 Plan evaluator
```

双执行器设计。

---

## 19. 关键回归测试：FTS 命中但 Locator 为空

使用当前 tokenizer 与 Locator 存在语义差异的情况构造测试。

例如：

```text
正文：
文、档

查询：
文档
```

如果真实 FTS tokenizer 判断命中，则必须：

```rust
let response = search(...)?;

assert_eq!(response.documents.len(), 1);
```

即使：

```rust
assert!(response.documents[0].hits.is_empty());
```

也允许。

核心断言：

> FTS 命中文档不会因为 Locator 无 span 而消失。

---

## 20. 第二个关键回归：只有部分 literal 被定位

构造：

```text
query:
合同 发票

content:
合、同……发票
```

如果当前 FTS MATCH 判定成立，则结果必须保留。

即使 Locator：

```text
合同 → 未定位
发票 → 已定位
```

也不能过滤文档。

---

## 21. 第三个关键回归：正常展示不回归

普通：

```text
query:
合同

content:
双方签订合同。
```

继续保证：

```text
FTS 命中
标题/正文高亮正常
Hit 正常
右侧定位正常
```

---

## 22. 第四个关键回归：结构化过滤

例如：

```text
MATCH 合同
+
file_type = pdf
```

只有 PDF 结果进入 SearchResponse。

“FTS 唯一判定”仅指：

> 全文文本条件由 FTS 唯一判定。

普通 SQL 结构化过滤仍然正常生效。

---

## 23. 第五个关键回归：content_hash 分组

相同内容多个路径继续保持：

```text
一个内容组
+
多个文件位置
```

不得因本次改造破坏。

---

## 24. 测试中删除旧预期

查找现有测试中类似：

```text
FTS 匹配
但 locate_literals 不满足旧字符串语义
→ 文档应被过滤
```

的断言。

若该测试验证的确实是旧“二次精确复核”机制，应改为：

```text
FTS MATCH 成立
→ 文档必须保留
```

不要机械修改 parser/compiler 自身的正确性测试。

---

## 25. 日志

FTS 命中但展示定位为空时，可选记录 debug：

```text
FTS result has no display spans:
document_id=...
```

不要使用 warning/error。

因为在当前 tokenizer 与 Locator 规则不同的情况下，这种情况可能正常出现。

---

## 26. 本次搜索决策单一化无法解决的问题

本章节必须作为本任务的边界说明保留。

这些问题在本次改造完成后仍然存在，**不得把本次任务描述成“全文检索问题全部解决”**。

---

### 26.1 Query Parser / Compiler 本身仍可能生成错误查询

搜索决策单一化只能保证：

```text
FTS5 是唯一执行者
```

但不能保证：

```text
交给 FTS5 的 MATCH 表达式一定正确
```

如果 `query.rs` 本身把用户查询编译错了，FTS5 仍会忠实执行一个错误查询。

因此以下问题本次不会自动解决。

#### B4：NOT 编译错误

例如：

```text
合同 -文档 -草稿
```

如果被错误编译成：

```text
合同 NOT (文档 AND 草稿)
```

而不是：

```text
合同 AND NOT 文档 AND NOT 草稿
```

最终搜索结果仍然错误。

这是后续 **Query Compiler 修复任务**。

#### B5：字段作用域覆盖错误

例如复杂：

```text
title:(content:合同)
```

如果 parser 错误覆盖内层字段，最终 MATCH 就已经错误。

搜索决策单一化无法解决。

后续要么：

1. 修复字段继承/覆盖规则；
2. 或收缩复杂字段语法。

#### B7：jieba 宽松模式产生无效条件

例如宽松查询：

```text
文、档
```

jieba 可能切出纯标点段：

```text
文
、
档
```

而当前 FTS tokenizer 不索引该标点，可能生成零词元或恒不成立条件。

本次仍然不会解决。

后续应单独修复：

```text
Query Rewrite / jieba 输出清洗
```

---

### 26.2 `total_hits` 计数仍可能错误

当前存在：

```text
真实 fragment 数
→ 截断到前 N 个 Hit
→ total_hits = hits.len()
```

的问题。

例如：

```text
真实 57 个命中 fragment
最大保存 20 个 Hit
```

当前可能：

```text
total_hits = 20
```

而不是：

```text
57
```

搜索决策单一化不会改变 fragment 生成逻辑。

因此：

> B2 仍存在。

后续单独修复：

```rust
FragmentResult {
    hits,
    total,
}
```

其中 `total` 必须在展示 limit 截断前计算。

---

### 26.3 高亮和命中导航仍可能遗漏

当前 `locate_literals()` / `find_ignoring_whitespace()` 存在：

```text
同一个 literal 的跨空白形式只定位第一处
```

等问题。

例如：

```text
文 档……文 档
```

可能只能定位第一处。

本次改造后：

```text
文档仍然可以正确出现在结果集中
```

但 UI 可能：

```text
少高亮一处
命中 N 处偏少
上一批/下一批缺少导航点
```

因此：

> B3 不会被本次修复。

区别只是它已经从：

```text
搜索正确性问题
```

降级为：

```text
展示/导航问题
```

后续再单独修 Locator。

---

### 26.4 FTS tokenizer 与 Display Locator 语义仍然不一致

本次明确：

```text
FTS 的语义为搜索真值
```

但不会立即让：

```text
locate_literals()
```

采用完全相同的 tokenizer 规则。

因此仍可能存在：

```text
FTS 命中
↓
Locator 找不到对应位置
↓
结果存在，但没有高亮
```

例如当前可能存在：

```text
查询：
文档

原文：
文、档
```

FTS 与 Locator 的判断不同。

本次正确处理方式只是：

```text
保留搜索结果
允许高亮为空
```

并没有解决“为什么高亮为空”。

后续建议单独实施：

> Display Locator 与 FTS tokenizer 语义对齐。

但不得重新给予 Locator 搜索否决权。

---

### 26.5 当前 tokenizer 的产品语义没有改善

本任务完全不修改 tokenizer。

因此以下问题仍然维持现状：

```text
A4 是否应拆为 a + 4
ASCII token 边界是否合理
doc 是否应该命中 document
标点是否应该打断 phrase
纯符号是否应可搜索
emoji 是否应可搜索
```

这些属于：

> tokenizer / 产品搜索语义设计问题。

必须作为独立任务评估。

不要把它们误认为搜索决策单一化会解决。

---

### 26.6 FTS 自身的欠召回不会由应用层补回来

采用单一决策以后，正式接受：

> FTS 没有命中的文档，就是 rsou 没有命中的文档。

例如如果当前 tokenizer 规定：

```text
doc
```

不会命中：

```text
document
```

那么本次不会再通过应用层字符串扫描去“补召回”。

这不是本次改造后的 bug，而是：

> 当前 tokenizer 定义的正式搜索行为。

如果认为这种行为不符合产品需求，应后续修改 tokenizer/查询规则，而不是重新引入第二套搜索决策器。

---

### 26.7 BM25 排名质量没有改善

当前：

```text
bm25(5.0, 1.0)
```

的：

```text
title/content 权重
逐字 tokenizer 带来的文档长度统计
大型 Excel 等长文档排序表现
```

本次全部不调整。

搜索决策单一化只保证：

```text
谁决定命中
```

并不保证：

```text
结果排序一定最优
```

后续可独立做 Ranking Evaluation。

---

### 26.8 `MAX_CANDIDATES / 200` 历史限制仍存在

本次为了保持最小改造，允许继续保留：

```text
最多处理前 200 个排名内容组
```

只是它不再是：

```text
候选复核预算
```

而是：

```text
当前结果处理/展示上限
```

因此如果以后需要：

```text
展示超过 200
分页
加载更多
深翻结果
```

仍然需要单独重构。

搜索决策单一化本身不会提供分页能力。

---

### 26.9 “命中 N 处”的产品定义没有重新设计

当前“命中 N 处”实际更接近：

```text
命中 fragment / chunk 批次数
```

而不是：

```text
关键词真实出现 N 次
```

本次继续沿用。

因此以下三个概念仍然不同：

```text
关键词 occurrence 数
高亮 Span 数
Hit / fragment 数
```

后续如果要改善 UI 文案或统计口径，应单独设计。

---

### 26.10 标题命中但正文无 Hit 的展示问题仍可能存在

FTS 可能因为标题命中而返回文档：

```text
title → hit
content → no hit
```

此时：

```text
hits.len() == 0
```

如果 UI 当前机械显示：

```text
命中 0 处
```

体验仍然不理想。

本任务可以做最小退化：

```text
标题命中
```

但不要求重新设计整个计数模型。

---

### 26.11 当前全文预览性能问题不在本任务范围

现在右侧点击结果仍可能：

```text
读取整篇 plain_text
↓
大文档再截取约 200KB 窗口
```

对于特别大的 Excel / PDF 转文本，可能有：

```text
数据库大字段读取
字符串分配
预览定位扫描
```

成本。

搜索决策单一化不优化这一部分。

后续若有实际性能问题，再独立优化 Preview Pipeline。

---

## 27. 本次改造后问题应如何重新分类

完成后应把问题分成三层。

| 层级  | 谁负责                             | 示例                   |
| --- | ------------------------------- | -------------------- |
| 查询层 | query.rs / jieba / 同义词          | NOT、字段、无效 token      |
| 搜索层 | FTS5                            | MATCH、BM25、filter、排名 |
| 展示层 | Locator / Hit / chunk / Preview | 高亮、计数、导航             |

其中：

> **只有搜索层决定文档是否属于结果。**

查询层如果生成错误 MATCH，会影响搜索结果，需要后续优先修复。

展示层即使有 bug，也不得影响结果集合。

---

## 28. 后续任务建议顺序

本任务完成后建议按以下顺序继续，而不是混进当前提交。

### Follow-up 1：Query 正确性

修复：

```text
B4 NOT
B5 字段作用域
B7 jieba 无效 token
```

因为这些仍直接影响唯一的 FTS 搜索决策。

---

### Follow-up 2：Display Locator

修复：

```text
B3 全量定位
FTS tokenizer 与 Locator 语义尽量对齐
字段感知高亮
OR/同义词展示一致性
```

只改善展示，不参与判真。

---

### Follow-up 3：Hit / Count

修复：

```text
B2 total_hits
标题独占命中文案
occurrence/span/fragment 计数定义
```

---

### Follow-up 4：Tokenizer / Ranking

独立评估：

```text
A4 token
ASCII 边界
标点
emoji
前缀/子串
BM25 权重
大型 Excel 排名
```

不要重新引入应用层第二裁判。

---

## 29. 错误处理

以下属于展示层错误：

```text
chunk_ranges 查询失败
Locator 无 span
fragment 为空
context_header 缺失
```

原则：

> 尽量降级展示，不要改变 FTS 已确定的搜索真假。

数据库正文读取等真正的存储错误继续沿用现有错误处理方式。

---

## 30. 完成标准

### 30.1 架构

代码中不存在：

```text
FTS MATCH 命中
↓
因为 highlight/span/hit 为空
↓
continue / retain(false)
```

### 30.2 注释

`locate_literals()` 不再被描述为：

```text
精确复核器
```

而是：

```text
展示定位器
```

### 30.3 UI

FTS 命中的文档即使没有展示 Span：

```text
仍出现在结果列表
仍可点击
仍可查看右侧原文
```

### 30.4 测试

至少覆盖：

1. FTS 命中 + Locator 为空；
2. FTS 命中 + 部分 literal 被定位；
3. 普通命中 + 正常高亮；
4. SQL metadata filter；
5. content_hash 分组。

---

## 31. 最终目标流程

本任务完成后：

```text
1. query.rs / jieba / 同义词生成 FTS MATCH
2. FTS5 + SQL filter 得到最终命中文档
3. BM25 / content_hash 进行排名与分组
4. 对准备展示的文档读取正文
5. Locator / chunks / Hit 生成展示数据
6. 返回 SearchResponse
```

严禁重新出现：

```text
7. 根据展示数据重新决定文档到底算不算命中
```

---

## 32. 一句话验收

如果维护者问：

> “FTS 搜到了，但 `locate_literals()` 一个高亮都没找到，这篇文档还返回吗？”

答案必须始终是：

> **返回。FTS 已经决定它命中，Locator 只负责展示。**

同时必须明确：

> **这次改造只解决搜索决策双重化，不解决 query compiler、tokenizer、高亮定位、命中计数、排名和分页等其他独立问题；这些已经在本文列明，留待后续任务处理。**

# ski_eval_rs/src/main.rs メモリ最適化レポート

## 対象コードの概要

対象は entity["company","GitHub","code hosting platform"] 上の `ski_eval_rs/src/main.rs` で、(a) SKI（S/K/I）コンビネータの遅延グラフ簡約（Arena 上にノードを確保し、IND で共有/更新）、(b) compact 形式のパース、(c) 出力データの I/O インタプリタ、(d) 画像（四分木/ダイアモンド構造）のレンダリング（特に深さ 1〜8 と、中央 1/2 をズームし続ける深さ 9〜25）を１ファイルで抱えています。citeturn25view0turn22view0

メモリ面の支配項は `Arena { nodes: Vec<Node>, ... }` の巨大な `nodes`（ノード総数）で、デコード補助関数やペア抽出が「細かい一時ノードの大量確保」を誘発しやすい構造です。citeturn25view0

## 推奨変更のエグゼクティブサマリー

最優先（低リスク・高インパクト）として、**(1) 画像レンダリングを“必要範囲のみ”に固定し続ける（深さ 9〜25 で 128×128 の中央ズームのみを描く）**、**(2) `decode_bool`・`pair_fst/snd` などのホット関数が毎回確保している「定数ノード（K/I/KI）とマーカー」を Arena 内で一度だけ確保して再利用**、**(3) `whnf` の spine（スタック）Vec を毎回 new/with_capacity せず Arena 内 scratch で使い回す**、の 3 点が “まず勝てる” 変更です。citeturn25view0turn22view0turn23search0

その次（中リスク・中〜高インパクト）として、**(4) GC のマークビットセット/スタックを Arena 内で再利用して、GC 実行時の一時メモリスパイクを抑える**、**(5) `keyfind` の `arena.nodes.clone()` のような「巨大 clone」を checkpoint/restore 方式に置換**が効きます（使うなら）。citeturn25view0turn22view0

上級（高リスク・高インパクト）として、**(6) `Node` を 12B→8B 近辺へ詰める（packed 表現）**、**(7) ノード ID の世代管理や free-list の最適化**は、実装コスト/バグリスクが上がるので、計測で `nodes` が真にボトルネックだと確信してからでOKです（後述の計測で判断）。citeturn23search3turn24search2

## ヒープ使用のホットスポット分析

### Arena と簡約器が作るメモリ特性

`Arena::alloc` が `nodes.push(Node {..})` を行い、評価が進むほど `nodes` が増え続けます。free list を使う設計ですが、**“一時的に作って捨てたいノード”**を大量に作る経路（デコード補助/レンダリング）だと、`nodes` の総量は簡単に膨らみます。citeturn25view0

`Arena::whnf` は局所 `Vec<u32>`（spine）を `Vec::with_capacity(256)` で毎回確保しています。これは「呼び出し回数が多いほど、アロケータへの負荷（確保/解放・メタデータ・断片化）」が積み上がります。citeturn25view0turn23search0

`Arena::gc` は呼ばれるたびに `(len+63)/64` の `Vec<u64>` を新規確保しており、ノードが大きいほど GC 実行時に一時メモリが跳ねます（コメントでも 500M ノードで ~62.5MB 相当と示唆）。citeturn25view0turn23search3

### デコード補助（bool/ペア抽出）が “小さい確保” を無限に積む

`decode_bool` は **毎回** marker ノード 2 個と application ノード 2 個（計 4 ノード）を確保してから `whnf` を回しています。I/O 解析やレンダリングで `decode_bool` が大量に呼ばれると、これだけで `nodes` の増分が支配的になり得ます。citeturn25view0turn10view0

同様に `pair_fst/pair_snd` と `pair1_fst/pair1_snd` が、**毎回** K/I/KI（あるいは dummy）を `alloc` してから applicative に評価しています。ペア抽出は数値/文字列/画像デコードの最内周にいるので、ここを“無駄確保ゼロに寄せる”のはメモリにも速度にも効きます。citeturn10view0

### 画像レンダリング（深さ 9〜25 中央ズーム）の要点

対象コードにはすでに「画像出力（p2=2）時、深さ 1〜8 は 1〜128、深さ 9〜25 は中央 1/2 をズームして描く」戦略が入っています。さらに `checkpoint/restore` を使って **ピクセル単位で一時ノードを破棄**し、メモリ上限を抑える方針も見えます。citeturn22view0turn25view0

ただし現状は、深さ 9〜25 のレンダリングが **16×16** になっていたり、ズームステップが深さの対応とズレる可能性があり、あなたが提示した「深さ 9〜25 は 128×128、中央の 1/2 を延々ズーム」の仕様に合わせるには調整が必要です。citeturn22view0

### ヒープ使用箇所の一覧（関数レベル、行番号つき）

下表は “ヒープを増やしやすい箇所” を、**原本の行番号**ベースで要約したものです（行範囲は関数定義のレンジ）。  

| 箇所（関数/行） | 主なヒープ使用 | 影響 | 低リスク施策 |
|---|---|---|---|
| `Arena::alloc` (L51–68) | `nodes.push` / `free_list.pop` | `nodes` が支配的に増える | 一時ノードを作る箇所を checkpoint/定数再利用で削る citeturn25view0 |
| `Arena::whnf` (L185–311) | `Vec::with_capacity(256)` を毎回 | アロケータ負荷↑ | spine を Arena フィールドで再利用（smallvec も可） citeturn25view0turn23search10 |
| `Arena::gc` (L103–151) | `vec![0u64; ..]` を毎回 | GC時スパイク | mark/stack を Arena に保持し再利用 citeturn25view0turn23search3 |
| `decode_bool` (L378–390) | marker2 + APP2 を毎回確保 | 呼び出し回数が多い | marker を使い回し、結果判定は node id 比較 citeturn25view0turn10view0 |
| `pair_fst/snd` (L3507–3525) | K/I/KI/dummy を毎回確保 | 内側ループ | K/I/KI を Arena にキャッシュ citeturn10view0 |
| `decode_church_num` (L3547–3586) | marker2 + APP2 を毎回確保 | 反復時に増える | marker をキャッシュ citeturn10view0 |
| 画像出力（`io` 内、深さ 9–25） | `Vec<u8>` の画像、HashMap キャッシュ、ピクセル毎の一時ノード | 仕様ズレで時間・出力不一致 | 深さ 9–25 を 128×128 固定 + ズームステップを 1 回/深さに citeturn22view0 |

## 具体的なコード編集案

ここでは「コンパイル可能・差分が小さい」ことを重視し、**(A) 定数/マーカーの Arena 内キャッシュ**、**(B) whnf spine の再利用**、**(C) GC の buffer 再利用**、**(D) 深さ 9〜25 の 128×128 中央ズーム化**を、最小差分で提示します。

### 中央ズームのデータフロー図

```mermaid
flowchart TD
  R[diamond quadtree root: data] -->|depth 1..8| P1[render: size = 1..128]
  P1 -->|output| IMG1[PGM files]

  R --> Z0[zoom_roots = [TL,TR,BL,BR]]
  Z0 -->|for depth=9..25| ZS[zoom step: TL<-BR, TR<-BL, BL<-TR, BR<-TL]
  ZS --> R128[render 128x128 with checkpoint/restore]
  R128 --> IMG2[PGM files depth9..25]
```

### 差分（定数/マーカーのキャッシュ + whnf/GC の buffer 再利用）

以下は「毎回 alloc していた K/I/KI や marker を一度だけ確保」し、`decode_bool` とペア抽出を軽くする差分です。`Vec::with_capacity` による先行確保の意味（リサイズ回避）も根拠として押さえておきます。citeturn23search0turn24search0

```diff
@@
 struct Arena {
     nodes: Vec<Node>,
     free_list: Vec<u32>,
     gc_roots: Vec<u32>,  // external roots for GC
     // Checkpoint/restore for per-pixel rendering
     checkpoint: Option<usize>,      // arena length at checkpoint
     saved_nodes: Vec<(u32, Node)>,  // base nodes modified since checkpoint
+
+    // === Cached constants/markers (kept alive via `gc_roots`) ===
+    k0: u32,
+    s0: u32,
+    i0: u32,
+    ki0: u32,              // false = K I (useful as selector)
+    bool_marker_t: u32,    // marker for decode_bool(true)
+    bool_marker_f: u32,    // marker for decode_bool(false)
+    church_f_marker: u32,  // marker for decode_church_num(f)
+    church_x_marker: u32,  // marker for decode_church_num(x)
+
+    // === Scratch buffers ===
+    spine: Vec<u32>,
+    gc_mark: Vec<u64>,
+    gc_stack: Vec<u32>,
 }
@@
 impl Arena {
     fn new(capacity: usize) -> Self {
-        Arena {
-            nodes: Vec::with_capacity(capacity),
-            free_list: Vec::new(),
-            gc_roots: Vec::new(),
-            checkpoint: None,
-            saved_nodes: Vec::new(),
-        }
+        let mut arena = Arena {
+            nodes: Vec::with_capacity(capacity + 16),
+            free_list: Vec::new(),
+            gc_roots: Vec::new(),
+            checkpoint: None,
+            saved_nodes: Vec::new(),
+
+            k0: NIL, s0: NIL, i0: NIL, ki0: NIL,
+            bool_marker_t: NIL, bool_marker_f: NIL,
+            church_f_marker: NIL, church_x_marker: NIL,
+
+            spine: Vec::with_capacity(256),
+            gc_mark: Vec::new(),
+            gc_stack: Vec::with_capacity(1024),
+        };
+
+        arena.k0 = arena.alloc(K, NIL, NIL);
+        arena.s0 = arena.alloc(S, NIL, NIL);
+        arena.i0 = arena.alloc(I, NIL, NIL);
+        arena.ki0 = arena.alloc(APP, arena.k0, arena.i0);
+
+        arena.bool_marker_t = arena.alloc(100, NIL, NIL);
+        arena.bool_marker_f = arena.alloc(101, NIL, NIL);
+        arena.church_f_marker = arena.alloc(110, NIL, NIL);
+        arena.church_x_marker = arena.alloc(111, NIL, NIL);
+
+        arena.gc_roots.extend_from_slice(&[
+            arena.k0, arena.s0, arena.i0, arena.ki0,
+            arena.bool_marker_t, arena.bool_marker_f,
+            arena.church_f_marker, arena.church_x_marker,
+        ]);
+
+        arena
     }
@@
-    fn gc(&mut self, roots: &[u32]) -> (usize, usize, usize) {
-        let len = self.nodes.len();
-        let mut marked = vec![0u64; (len + 63) / 64];
+    fn gc(&mut self, roots: &[u32]) -> (usize, usize, usize) {
+        let len = self.nodes.len();
+        let need_words = (len + 63) / 64;
+        if self.gc_mark.len() != need_words {
+            self.gc_mark.resize(need_words, 0u64);
+        } else {
+            for w in self.gc_mark.iter_mut() { *w = 0; }
+        }
+        let marked: &mut [u64] = &mut self.gc_mark;
@@
-        let mut stack: Vec<u32> = Vec::with_capacity(1024);
+        self.gc_stack.clear();
         for &r in roots.iter().chain(self.gc_roots.iter()) {
             if (r as usize) < len && !is_marked(&marked, r) {
-                stack.push(r);
+                self.gc_stack.push(r);
             }
         }
-        while let Some(idx) = stack.pop() {
+        while let Some(idx) = self.gc_stack.pop() {
@@
     fn whnf(&mut self, node: u32, fuel: &mut u64) -> u32 {
-        let mut spine: Vec<u32> = Vec::with_capacity(256);
+        self.spine.clear();
+        let spine: &mut Vec<u32> = &mut self.spine;
         let mut n = self.follow_mut(node);
```

※ `Vec::with_capacity` / `HashMap::with_capacity` は「指定した容量までは realloc を避けられる」性質なので、こういった scratch やキャッシュに向いています。citeturn23search0turn24search0

### 差分（decode_bool / ペア抽出を “定数再利用” に寄せる）

```diff
@@
 fn make_false(arena: &mut Arena) -> u32 {
-    let k = arena.alloc(K, NIL, NIL);
-    let i = arena.alloc(I, NIL, NIL);
-    arena.alloc(APP, k, i)
+    arena.alloc(APP, arena.k0, arena.i0)
 }
@@
 fn make_true(arena: &mut Arena) -> u32 {
-    let k1 = arena.alloc(K, NIL, NIL);
-    let k2 = arena.alloc(K, NIL, NIL);
-    let kk = arena.alloc(APP, k1, k2);
-    let s = arena.alloc(S, NIL, NIL);
-    let skk = arena.alloc(APP, s, kk);
-    let i = arena.alloc(I, NIL, NIL);
-    arena.alloc(APP, skk, i)
+    let kk = arena.alloc(APP, arena.k0, arena.k0);
+    let skk = arena.alloc(APP, arena.s0, kk);
+    arena.alloc(APP, skk, arena.i0)
 }
@@
 fn decode_bool(arena: &mut Arena, node: u32, fuel: u64) -> Option<bool> {
-    let marker_t = arena.alloc(100, NIL, NIL);
-    let marker_f = arena.alloc(101, NIL, NIL);
-    let app1 = arena.alloc(APP, node, marker_t);
-    let app2 = arena.alloc(APP, app1, marker_f);
+    let app1 = arena.alloc(APP, node, arena.bool_marker_t);
+    let app2 = arena.alloc(APP, app1, arena.bool_marker_f);
     let mut f = fuel;
     let result = arena.whnf(app2, &mut f);
     let result = arena.follow(result);
-    let tag = arena.nodes[result as usize].tag;
-    if tag == 100 { Some(true) }
-    else if tag == 101 { Some(false) }
-    else { None }
+    if result == arena.bool_marker_t { Some(true) }
+    else if result == arena.bool_marker_f { Some(false) }
+    else { None }
 }
@@
 fn pair_fst(arena: &mut Arena, node: u32, fuel: &mut u64) -> u32 {
-    let k_sel = arena.alloc(K, NIL, NIL);
-    let app1 = arena.alloc(APP, node, k_sel);
-    let dummy = arena.alloc(I, NIL, NIL);
-    let app2 = arena.alloc(APP, app1, dummy);
+    let app1 = arena.alloc(APP, node, arena.k0);
+    let app2 = arena.alloc(APP, app1, arena.i0);
     arena.whnf(app2, fuel);
     arena.follow(app2)
 }
@@
 fn pair_snd(arena: &mut Arena, node: u32, fuel: &mut u64) -> u32 {
-    let ki = make_false(arena);
-    let app1 = arena.alloc(APP, node, ki);
-    let dummy = arena.alloc(I, NIL, NIL);
-    let app2 = arena.alloc(APP, app1, dummy);
+    let app1 = arena.alloc(APP, node, arena.ki0);
+    let app2 = arena.alloc(APP, app1, arena.i0);
     arena.whnf(app2, fuel);
     arena.follow(app2)
 }
```

この “定数/マーカーのキャッシュ化” は、`decode_bool`/`pair_*` が大量に呼ばれるルートで `nodes` の増分を直接削るので、メモリにも時間にもわりと素直に効きます。citeturn25view0turn10view0

### 差分（深さ 9〜25 の中央ズームを仕様通り 128×128 に固定）

あなたのメモにある「深さ 1〜8 は 1×1〜128×128、深さ 9〜25 は毎回“中央の 1/2”だけをズームして 128×128」へ合わせるなら、`io` の画像出力（p2=2）内 Phase 2 は “1 深さにつき 1 回だけズーム”し、描画サイズを 128 に固定するのが一番まっすぐです。citeturn22view0

```diff
@@
-// Phase 2: Center zoom for depths 9-25
-eprintln!("  Phase 2: Center zoom depths 9-25...");
-let mut zoom_tl = root_tl;
-let mut zoom_tr = root_tr;
-let mut zoom_bl = root_bl;
-let mut zoom_br = root_br;
-
-// Do zoom steps 1-8 first (without rendering, just navigate to center)
-for _step in 1..=7 { ... }
-
-for depth in 9..=max_depth {
-    // zoom step
-    ...
-    let render_sz: usize = 16;
+// Phase 2: Center zoom for depths 9-25
+eprintln!("  Phase 2: Center zoom depths 9-25...");
+let mut zoom: [u32; 4] = [root_tl, root_tr, root_bl, root_br];
+
+for depth in 9..=max_depth {
+    // Apply exactly one center-zoom step per depth.
+    zoom = [
+        get_child_fn(&mut arena, zoom[0], 4, &sels, &mut child_cache, eval_fuel),
+        get_child_fn(&mut arena, zoom[1], 3, &sels, &mut child_cache, eval_fuel),
+        get_child_fn(&mut arena, zoom[2], 2, &sels, &mut child_cache, eval_fuel),
+        get_child_fn(&mut arena, zoom[3], 1, &sels, &mut child_cache, eval_fuel),
+    ];
+    let zoom_tl = zoom[0];
+    let zoom_tr = zoom[1];
+    let zoom_bl = zoom[2];
+    let zoom_br = zoom[3];
+
+    let render_sz: usize = 128;
     let pix = render_with_checkpoint(
         &mut arena,
         [zoom_tl, zoom_tr, zoom_bl, zoom_br],
         render_sz, &sels, eval_fuel
     );
 }
```

この修正は「ズームの対応関係を明確にしつつ、範囲外描画をしない」方針に一致します（= 深さ 9 以降はビュー自体が“中央だけ”に更新されるので、外周はそもそも描かない）。citeturn22view0

## 変更ごとの概算メモリ削減量と優先度

※ ここでの試算は「`Node { u8, u32, u32 }` はアラインメント込みでおおむね 12B」程度と仮定したざっくり推定です（実測は後述の計測で確定するのが安全）。citeturn23search3turn24search2

| 変更 | 優先度 | リスク | 期待効果（メモリ） | どう効くか |
|---|---|---|---|---|
| 深さ 9〜25 を 128×128 の中央ズームだけに固定 | 最優先 | 低 | “指数爆発”回避（実質無限大の削減） | 全展開/巨大バッファを作らない設計に寄せる citeturn22view0 |
| `decode_bool` の marker 再利用（2ノード/呼び出し削減） | 最優先 | 低 | 呼び出し回数×(約24B) | I/O/レンダリングでの `nodes` 増分が直接減る citeturn10view0turn25view0 |
| `pair_fst/snd`・`pair1_*` の K/I/KI 再利用（2〜4ノード/呼び出し削減） | 最優先 | 低〜中 | 呼び出し回数×(約24〜48B) | ペア抽出は内側ループなので効きやすい citeturn10view0 |
| `whnf` の spine Vec 再利用（“毎回1KB確保”を除去） | 高 | 低 | peak より “断片化/アロケータ負荷” 減 | `Vec::with_capacity` の繰り返しを潰す citeturn25view0turn23search0 |
| GC mark/stack の再利用 | 中 | 低 | GC 実行時の一時スパイク減 | 大きい `vec![..]` を繰り返し確保しない citeturn25view0turn23search3 |
| `keyfind` の `arena.nodes.clone()` を checkpoint 方式へ | 中（使うなら） | 中 | 数百MB〜GB 級のピーク減 | “丸ごと複製”をやめる citeturn25view0turn10view0 |

## 代替データ構造・手法とトレードオフ

`Node` のメモリを本当に削り切るなら、「`Vec<Node>` をやめて packed 表現（例: `u64` に tag と child を詰める）」は強力です。`Vec<Node>` のままより 25〜35% 程度の削減余地が出やすい一方で、ビット操作増加・デバッグ難化・バグの混入が起きやすく、CPU と開発コストのトレードオフが大きいです（高リスク）。citeturn23search3turn24search15

`whnf` の spine は `smallvec::SmallVec<[u32; N]>` にすると、典型ケースでヒープ確保をゼロにできる可能性があります。ただし crate 追加が必要になり（`Cargo.toml` 変更が別途必要）、依存増もあるので、まずは Arena 内 `Vec` 再利用で十分です。citeturn23search10turn23search2

文字列や引数処理の `String` clone を `&str` 参照や `Cow<str>` に寄せるのも可能ですが、ここは全体の支配項になりにくいので “見た目の綺麗さ” 以上の効果は限定的です。とはいえ `Cow` は clone-on-write で「必要になるまで所有しない」用途向けなので、ログ/設定値の扱いには相性が良いです。citeturn23search1

`HashMap` キャッシュは、エントリ数が読めるなら `with_capacity` が有効です（リハッシュ回数/再確保抑制）。ただし本件ではキャッシュサイズが小さめに見えるので、優先度は低めでもOKです。citeturn24search0

## 計測・ベンチ・テスト計画

### ベンチマーク観点と “取るべき指標”

このコードは「計算量（簡約ステップ）」「Arena ノード数」「一時確保の回数」の 3 つが絡むので、次の指標が特に有効です。

- `arena.nodes.len()` の推移（深さごと、レンダリング行ごと、I/O ステップごと）
- `arena.free_list.len()` の推移（GC/再利用効率の目安）
- 画像 1 枚あたりの wall time（深さ 1〜8、9〜25）
- 総割り当て回数/総割り当てバイト/ピークヒープ（heap profiler で）
- （可能なら）ピーク RSS（OS の resident set）

Massif は「ピークヒープと内訳」を見るのに向き、heaptrack は「どこが何回 alloc してるか」を見るのに向きます。DHAT は “ヒープ割当の性質” を深掘りできます。citeturn23search3turn24search2turn24search19

### 実行・計測コマンド例

`cargo bench` はベンチ実行コマンドで、`--` 以降はベンチバイナリに渡る、という基本仕様を押さえておくと使いやすいです。citeturn24search1

- ベンチ（もし `benches/` や criterion 等を用意するなら）
  ```bash
  cargo bench -- --nocapture
  ```

- Massif（Valgrind）
  ```bash
  valgrind --tool=massif --massif-out-file=massif.out \
    target/release/ski_eval_rs <INPUT> --decode io --key 5,0,17,5,3
  ms_print massif.out
  ```
  Massif はヒープ使用量（実使用＋オーバーヘッド）を測るツールで、ピーク解析に適しています。citeturn23search3turn23search7

- DHAT（Valgrind）
  ```bash
  valgrind --tool=dhat \
    target/release/ski_eval_rs <INPUT> --decode io --key 5,0,17,5,3
  ```
  DHAT はヒープ割当ブロックの性質（サイズ、寿命、アクセス等）を分析するためのツールです。citeturn24search19

- dhat-rs（Rust 側に小改造が許されるなら）
  ```rust
  // main の先頭付近
  let _profiler = dhat::Profiler::new_heap();
  ```
  `dhat` crate は Rust プログラムに組み込めるヒーププロファイラで、テストで “割当回数/ピーク” を閾値化する用途にも使えます。citeturn24search3turn24search11turn24search7

- heaptrack（Linux）
  ```bash
  heaptrack target/release/ski_eval_rs <INPUT> --decode io --key 5,0,17,5,3
  heaptrack_print heaptrack.*.gz | less
  ```
  heaptrack は LD_PRELOAD で alloc/free を追跡し、スタックトレース付きで記録します。citeturn24search2turn24search14

### ふるまい不変を確認するテスト案

現状はバイナリ1本なので、`main.rs` 内に `#[cfg(test)] mod tests { ... }` を追加する形が最小です。おすすめの “壊れやすい所を守るテスト” は以下です（外部データ不要で完結）。

- `decode_bool` が true/false を正しく返す（marker キャッシュに切り替えても）
- `pair_fst/pair_snd` が `make_pair` で作ったペアから要素を取り出せる
- `checkpoint/restore_checkpoint` が “一時の alloc と base 変更” を戻せる（レンダリングのメモリ境界が守られる）
- `build_diamond_sel` の selector が 5-tuple から期待フィールドを取れる（既存 selftest を unit test 化）

この手のテストは、後で最適化を追加しても “出力が変わってない” を守る土台になります。citeturn25view0turn22view0

### 実装タイムライン案

| フェーズ | 内容 | 期待効果 | 成果物 |
|---|---|---|---|
| すぐ | 深さ 9〜25 を 128×128・中央ズーム 1回/深さに修正 | 仕様一致・無駄描画削減 | 上記 diff（Phase2） citeturn22view0 |
| すぐ | `decode_bool` / `pair_*` の定数・マーカーを Arena で再利用 | `nodes` 増分と時間を削る | Arena キャッシュ diff citeturn25view0turn10view0 |
| 次 | `whnf` spine の再利用、GC バッファ再利用 | 断片化/スパイク抑制 | scratch fields diff citeturn25view0turn23search0turn23search3 |
| 計測後 | まだ `nodes` が限界なら packed Node を検討 | 常時メモリ大幅減（高リスク） | 別ブランチで POC citeturn23search3turn24search15 |

以上の変更が入ると、あなたが言ってた「深さ 5 あたりまで真っ黒」「以降は範囲外を打ち切りつつズームして深い所まで出力」が、メモリ的にも時間的にも現実的になるはずです。citeturn22view0turn25view0
# あんたがた2026HP Stars パズル解析プロジェクト

## このファイルについて
重要な情報を随時更新する。コンテキストが失われた場合に備え、発見事項・進捗・判断根拠をここに記録する。

## 作業方針
- 方針や実装の相談は、適宜 `codex exec` を利用してcodexと行うこと
- 意味のありそうな画像は `png/` フォルダ（imagesの外）にPNG形式で保存する

## プロジェクト概要
- `stars.txt` (~400MB) はSKIコンビネータ計算のプログラム
- 正しく評価すると: 質問文出力 → 鍵文字列入力 → 正誤判定 → (正解なら)画像出力
- 画像から最終回答を読み取るのがゴール
- コンパクト形式: `k`=S, `X`=K, `D`=I, `-`=application (後置記法/スタックマシン)

## 信頼度レベル
- `reference/hint-new.md` → **最新GMヒント（最重要・完全に信頼可能）**
- `reference/hint_old.md`, `reference/hint.md` → **旧GMヒント（信頼可能）**
- `reference/reference.md` → **プレイヤー解析（間違っている可能性あり）**
- 外部情報（他プレイヤーから得た情報）は検証の上で採用

## ファイル構成
```
very_large_txt/stars_compact.txt  — コンパクト形式 (30,485,221 bytes)
ski_eval_rs/src/main.rs           — Rust SKI評価器（メイン、~3200行）
analysis_charcode.md              — 文字コード解析メモ
scripts/                          — Python/JS解析スクリプト群
  ski_eval.py                     — Python版SKI評価器
  step4e_test_operators.py        — 演算子テスト
  test_server.js                  — サーバーテスト用
send/send.js                      — 他プレイヤーのサーバー提出スクリプト
extracted/left_x.txt              — LEFT部のコンパクト表現
reference/hint-new.md             — 最新GMヒント
reference/                        — その他ヒント・解析文書
images/                           — レンダリング出力
```

## ビルドと実行
```bash
# ビルド
cd ski_eval_rs && PATH="/c/Users/mizuki/.cargo/bin:$PATH" cargo build --release

# 実行（decodeモードを指定）
./ski_eval_rs/target/release/ski-eval.exe very_large_txt/stars_compact.txt --fuel 2000000000 --decode <MODE> --img images/output
```

### 主なdecodeモード
| モード | 説明 |
|--------|------|
| io | I/Oインタプリタ（質問出力→入力→応答→停止） |
| keyfind | タイミングサイドチャネル攻撃（鍵文字総当たり） |
| render, render2 | 画像レンダリング |
| examine, describe | 構造調査 |
| num, bool, list | 各種デコード |
| stream, structure, walk1 | 詳細構造解析 |

---

## SKIコンビネータ規則
- S f g x → f x (g x)
- K x y → x
- I x → x
- edge_mark (l): l A B C → A C (B (K C)) — stars.txtの原始プリミティブ

## エンコーディング仕様

### 2種類のpair
1. **pair1（1引数 Scott pair / タプル）**: `S(SI(KA))(KB)`
   - pair1(handler) = handler(A)(B)
   - pair1_fst: pair1(K) = A, pair1_snd: pair1(KI) = B
   - **用途**: I/O命令のタプル構造

2. **pair2（2引数 Scott pair / リスト用）**: `S(KK)(S(SI(KA))(KB))`
   - pair2(f)(g) = f(A)(B) — fがpairハンドラ、gがnilハンドラ
   - pair_fst: pair2(K)(dummy) = A, pair_snd: pair2(KI)(dummy) = B
   - **用途**: リスト（文字列、数値ビット列）のcons cell

### 真偽値
- true = S(KK)I → true(x)(y) = x
- false = KI → false(x)(y) = y
- nil = false = KI

### 数値 (2の補数、ビット列)
- pair(bit, rest_bits) のチェーン。pair_fst=bit、pair_snd=rest
- 0 = pair(false, nil), 1 = pair(true, pair(false, nil))
- 6 = pair(false, pair(true, pair(true, pair(false, nil))))
- 終端: pair(false, nil)

### 文字列リスト（Convention B — 実証済み）
- cons(prev_list, char_code) = pair2(prev_list, char_code)
  - **pair_fst = prev_list（残りのリスト）、pair_snd = char_code（文字コード）**
- nil (KI=false) でリスト終端
- ⚠ ヒントの記述とは逆順（pair_fst=value ではなく pair_fst=rest）だが、実際にこれで動作確認済み

### Church数（I/Oタグ専用）
- Church 0 = KI, Church 1 = S(S(KS)(S(KK)I))(KI)
- I/Oタグ (p1, p2) にのみ使用。整数とは別のエンコーディング。

---

## I/O プロトコル（検証済み）

### I/O命令の構造
```
IO命令 = pair1(pair1(p1, p2), Q)
```
- p1, p2: Church数（I/Oタグ）
- Q: データ/継続

### I/Oタグの意味
| p1 | 意味 | p2の意味 | Qの内容 |
|----|------|----------|---------|
| 0  | 停止 | 0        | 0 (全てゼロ) |
| 1  | 出力 | 0=整数, 1=文字列, 2=画像 | pair1(data, continuation) |
| 2  | 入力 | 0=整数, 1=文字列 | λx.continuation(x) |

### 確認済みI/Oフロー
空文字列入力時:
1. **Step 1**: 出力・文字列 (p1=1, p2=1) — 33文字の質問文
2. **Step 2**: 入力・文字列 (p1=2, p2=1) — 鍵文字列の入力要求
3. **Step 3**: 出力・文字列 (p1=1, p2=1) — 5文字のメッセージ（"wrong"）
4. **Step 4**: 停止 (p1=0, p2=0)

正しい鍵（QfnQ& = コード[5,0,17,5,3]）入力時:
1. **Step 1**: 出力・文字列 (p1=1, p2=1) — 33文字の質問文
2. **Step 2**: 入力・文字列 (p1=2, p2=1) — 鍵文字列の入力
3. **Step 3**: 出力・文字列 (p1=1, p2=1) — 鍵を5回繰り返し（25文字 = [5,0,17,5,3]×5）
4. **Step 4**: 出力・画像 (p1=1, p2=2) — **画像データ出力**
5. **Step 5**: 停止 (p1=0, p2=0)

---

## サーバー情報

### API
- URL: `https://stars-2026-hp.xyz/`
- Method: POST
- Content-Type: application/json
- Body: `{ "input": "<文字列>" }`
- Response: `{ "output": [{ "type": "string"|"image", "value": "..." }] }`

### サーバーの有効文字セット（28文字）
```
& * , - 0 5 9 < C D F M P Q W X [ f j k l n o u w y z {
```
- これら以外のASCII文字（a-z通常、A-Z通常、数字等）は無視される
- **全ての非空の有効入力に対して同一レスポンスが返る**（ブルートフォース不可）

### サーバー応答の例
```
空入力      → "cwnz eDlu8("
"M5PWz"     → "cQn9zn eM5PWz QPW9-8("
任意の有効文字 → "cQn9zn eM5PWz QPW9-8("  （M5PWzと同じ）
```

### サーバー表示文字とプログラム内部コードの対応
サーバーは独自の文字セット（上記28文字）でSKI式を表示する。
`M5PWz` は `wrong` のエンコード表現。

### サーバー記法の構造文字（reference.mdより確定）
| 記法 | 標準 | 役割 |
|------|------|------|
| `c` | `[` | 角括弧開き（式開始） |
| `(` | `]` | 角括弧閉じ（式終了） |
| `e` | `(` | 丸括弧開き（グループ化/シンボル） |
| `8` | `)` | 丸括弧閉じ |
| `j` | `"` | 文字列引用符 |
| `[` | `,` | カンマ区切り |
| `f` | `.` | ドット（名前空間区切り） |
| `o` | `?` | 疑問符 |

### 主要語彙（reference.mdより確定）
| 記法 | 英語 | | 記法 | 英語 |
|------|------|-|------|------|
| `zMnX` | view | | `kPX` | is |
| `znQPX` | answer | | `,lz` | for |
| `QnWPX` | print | | `QnQ` | of |
| `zlWPX` | encode | | `zlWlQD` | definition |
| `wnz` | what | | `M5PWz` | wrong |
| `MPQP` | result | | `zWP5M` | correct |
| `Qn9zn` | error | | `PX{nQ` | question |
| `-nQPz` | chapter | | `zMn5` | title |
| `Xnkl` | about | | `QP&PXMP5` | execute |

---

## 文字コード解析

### ⚠ 旧情報の訂正
以前の5文字マッピング（wrong=M5PWzからの推定）は**Convention B逆順のため不正確だった**。
サーバーへの直接クエリ（`charcode_query.js`）により、完全な対応表を取得済み。

### 完全な文字コード対応表（サーバークエリで確定）
**取得方法**: `[print [format [output.number [pop (CHAR) [z.1 z.2 z.1] 0] end]]]`
```
コード →  表示文字
  0   →  f        10  →  F        20  →  W
  1   →  w        11  →  *        21  →  M
  2   →  <        12  →  y        22  →  P
  3   →  &        13  →  ,        23  →  k
  4   →  9        14  →  C        24  →  X
  5   →  Q        15  →  {
  6   →  l        16  →  D        負のコード:
  7   →  u        17  →  n        -1  →  [
  8   →  -        18  →  0        -2  →  o
  9   →  5        19  →  z        -3  →  j
```

### 質問文（33文字、内部コード列）
```
2, 8, 16, 5, 6, 20, 6, 19, 8, 22, 8, 19, 22, 21, 8, 24, 22, 23, 8, 19, 17, 1, 8, 7, 5, 17, 1, 22, 5, 8, 19, 6, 13
```

### 質問文のデコード結果（確定）
コード→表示文字変換:
```
<-DQlWlz-P-zPM-XPk-znw-uQnwPQ-zl,
```
データチェーン順序（=正しい読み順）で逆転:
```
,lz-QPwnQu-wnz-kPX-MPz-P-zlWlQD-<
```
`-`（コード8）を単語区切りとすると:
```
,lz | QPwnQu | wnz | kPX | MPz | P | zlWlQD | <
```

### 質問文の翻訳（確定）
reference.mdの語彙表およびサーバー応答から:
| 表示文字 | 英語 | 根拠 |
|---------|------|------|
| `,lz` | for | reference.md語彙表 |
| `QPwnQu` | navigation | ナビゲーターページ識別子 |
| `wnz` | what | reference.md語彙表 + whatコマンド |
| `kPX` | is | reference.md語彙表 |
| `MPz` | current | ナビゲーター応答コンテキスト |
| `P` | ??? | （不明、冠詞的なもの?） |
| `zlWlQD` | definition | reference.md語彙表 |
| `<` | ? | （終端記号/疑問符相当?） |

**質問全文**: 「for navigation, what is [the] current definition ?」

### エラーメッセージ（5文字） = "wrong"
コード: 19(=z), 20(=W), 22(=P), 9(=5), 21(=M)
表示文字列: `M5PWz`（データチェーン逆順で読むと `zWP5M`、サーバー表示は `M5PWz`）

### 整数0-24のコンパクトSKI表現（サーバー記法）
```
Dlu=0, Xn&=1, PXz=2, n9u=3, zPQ=4, uPX=5
uPXn&=6, uPXPXz=7, uPXn9u=8, uPXzlQ=9
wn-=10, wn-Xn&=11, ..., wn-uPXzlQ=19
PXzn-wn-=20, ..., PXzn-wn-zPQ=24
```
- 5進法ベースの構造（0-4基本、5×n+mで拡張）

---

## ナビゲーター調査結果（サーバークエリで確定）

### ナビゲーターページ応答
クエリ: `czMnX ePXMn-nQ8(` = `[view (navigator)]`
```
[result [title (navigator)]
 [definition [(navigator)
   (for navigation QfnQ& is current definition)
   (you execute "[view [about command]]" link, command chapters also)
   (you execute "[view [about QfnQ&]]" link, QfnQ& chapters also)
   (you execute "[view [about dictionary]]" link, dictionary chapters also)]]]
```
→ **ナビゲーターが「current definition = QfnQ&」と回答**

### QfnQ& について
- `czMnX eXnkl QfnQ&8(` = `[view [about QfnQ&]]` で情報取得済み
- Q.0が最初の章: `(chapter Q.0 the_code and QfnQ&)`
- QfnQ& はQ問題シリーズの総合識別子/テーマ名
- ナビゲーターの3つのセクション: `QnzF0lX`(command), `QfnQ&`(Q-series), `Qn&n-QPznk`(the_graph)

### サーバーでの問題回答確認
- Q.0 の正答 = `PXz` (=2) → サーバーで **正解確認済み**
- Q.3 の正答 = `zPQ` (=4) → サーバーで **正解確認済み**

### 鍵文字列の候補
質問「for navigation, what is current definition?」の答え = **`QfnQ&`**
内部コード列: **[5, 0, 17, 5, 3]**
（Q=5, f=0, n=17, Q=5, &=3）

---

## 外部解析データ（他プレイヤー情報）

### グラフルート問1
```
クエリ: czMnX e-nQPz lfXn& QP5959PX QnQ Qn&n-QPznk8(
全文:   cMPQP czMn5 e-nQPz lfXn& QP5959PX QnQ Qn&n-QPznk8( czlWlQD cePX,P5 QPwF9zPX QnQ PX9M kPX QP5959PX QnQ &n-QPznk8 ezPX -nW Qnkn<lW uPXl-FXnuz QP5959PX QnQ Qn&n-QPznk8 cezPX -nW kn<lW 5959PX Qn&n-QPznk QPP9znX lf&n{QPX8
```
- サーバー表示文字で書かれた全質問テキスト
- サーバーAPI応答よりかなり長い → サーバーの返すテキストは省略/集約されている可能性
- 「8(」で終端するパターンが繰り返される
- 「c」で各出力ブロックが始まるパターン

---

## 試行済みアプローチと結果

### タイミングサイドチャネル攻撃（keyfind モード）
- 実装: I/O Step2(入力)のQ(λ)に各文字コード(1-24)を適用し、ステップ数で正しい文字を判別
- **結果: 失敗** — 遅延評価のため、Q(input)が即座にWHNFに達してしまう（~25000ステップ）
- 改良版: Q(input)の結果からタグ・Q2・データ・出力文字列の先頭文字まで強制評価
- 改良版の結果は未検証

### サーバーブルートフォース
- 全28有効文字の単一・二重・三重文字を試行
- **結果: 失敗** — 全ての非空有効入力に対して同一レスポンス

### 鍵文字列の検証（成功）
- **鍵: QfnQ& (コード [5,0,17,5,3])** — I/Oインタプリタで検証済み
- 文字列構築: B_fst_val convention + reverse push order
  - `make_pair(char_code, rest)` で pair_fst=value, pair_snd=rest
  - `key_codes.iter().rev()` で逆順にプッシュ（最初の文字が最も外側）
- 結果: Step 3で鍵を5回エコー、Step 4で画像出力(p2=2) → **"wrong"ではない = 正解**
- 実行コマンド:
```bash
./ski_eval_rs/target/release/ski-eval.exe very_large_txt/stars_compact.txt \
  --fuel 5000000000 --decode io --key 5,0,17,5,3 --img images/trykey --grid 64
```

### 画像レンダリング（進行中）
- **画像データはdiamond構造（Church-encoded 5-tuple）**
  - reference.mdより: `diamond = λabcdef. f a b c d e`
  - `diamond(COND)(QA)(QB)(QC)(QD)` = `λf. f(COND)(QA)(QB)(QC)(QD)`
  - QA=NW, QB=NE, QC=SW, QD=SE
- **pair1のK/KIセレクタではOOMになる** — 余分な引数が互いに適用されてしまう
- 正しい5引数セレクタ（検証済み）:
  - sel_0 = `S(KK)(S(KK)(S(KK)(S(KK)(I))))` → COND
  - sel_1 = `K(S(KK)(S(KK)(S(KK)(I))))` → QA (NW)
  - sel_2 = `K(K(S(KK)(S(KK)(I))))` → QB (NE)
  - sel_3 = `K(K(K(S(KK)(I))))` → QC (SW)
  - sel_4 = `K(K(K(K(I))))` → QD (SE)
- 抽出方法: `data(sel_i)` で i番目のフィールドを取得

#### OOM対策: mark-sweep GC + checkpoint/restore
- **mark-sweep GC実装済み**: ビットマップマーキング → スイープでfree_listに回収
- **checkpoint/restore機構実装済み**:
  - `set_checkpoint()`: 現在のarena長を記録、saved_nodesをクリア
  - `save_node(idx)`: whnfでノード変更前にcheckpoint以前のノードを保存
  - `restore_checkpoint()`: 保存ノードを復元、新規割当分をtruncate
  - checkpoint中はfree_listからの再利用を禁止（ベースノードの汚染防止）
  - whnfの全リダクション規則(I, K, K1, S, S1, S2)にsave_node呼出し追加済み
  - follow_mutもcheckpoint中はパス圧縮をスキップ
- **pixel-by-pixel レンダラー (`render_with_checkpoint`)**:
  - 各ピクセル: set_checkpoint → 四分木をナビゲート → bool_b抽出 → restore_checkpoint
  - メモリがベースアリーナサイズ(~31.8Mノード)に固定される（蓄積なし）
- **レンダリング2段階**:
  - Phase 1 (depth 1-8): 2^(depth-1)解像度でフル描画
  - Phase 2 (depth 9-25): 中央1/2にズーム、16x16で描画

#### 前回のズームプローブ結果（中央1x1の色）
| 深さ | TL | TR | BL | BR | 備考 |
|------|----|----|----|----|------|
| 9 | F | F | F | F | 全黒 |
| 10 | T | F | F | T | 対角パターン! |
| 11 | T | T | T | T | 全白 |
| 12 | T | T | T | T | 全白 |
| 13 | T | T | F | T | |
| 14 | F | T | F | T | |
| 15 | F | T | F | F | |
| 16 | F | F | F | T | |

→ 深さ10以降で非自明なパターンあり

#### 現在のレンダリング状況（2026-02-18 17:35実行開始）
- 実行コマンド: `--fuel 2000000000 --decode io --key 5,0,17,5,3 --img images/zoom`
- Phase 1: depth 1-6完了（全て真っ黒 = 正常、GMヒント通り）
- Phase 1: depth 7 (64x64) レンダリング中
- Arena: 31,842,011ノードで安定（checkpoint/restoreが正常動作）
- Phase 2 (depth 9-25の中央ズーム) は未着手

---

## 発見したバグ（修正済み）
1. pair encoding: 1引数 → 2引数 に修正
2. make_scott_num(0): KI → pair(false, nil) に修正
3. 数値終端: bare nil → pair(false, nil) に修正
4. pair extraction: 1引数適用 → 2引数適用に修正
5. decode_church_num: 部分式のwhnf強制が欠落 → 追加して修正

---

## 進捗チェックリスト
- [x] Step 1-4: 圧縮、パース、ラムダ変換、演算子抽出
- [x] Rust SKI評価器構築（グラフリダクション、fuel制限、arenaベース）
- [x] pair/数値エンコーディングのバグ修正
- [x] セルフテスト検証合格
- [x] I/Oインタプリタ実装・Church数デコード
- [x] I/Oフロー発見（出力→入力→出力→停止）
- [x] Convention B発見（pair_fst=rest, pair_snd=value）
- [x] 文字列デコード成功（33文字の質問文 + 5文字のエラーメッセージ）
- [x] 5文字の文字コード対応判明（wrong = M5PWz = コード19,20,22,9,21）
- [x] サーバーAPI調査（有効文字28種、全入力で同一応答）
- [x] **全文字コード対応の解明** — charcode_query.jsでサーバーに直接問い合わせて全28文字+負コード3文字を取得
- [x] **質問文の完全解読** — `,lz-QPwnQu-wnz-kPX-MPz-P-zlWlQD-<` = "for navigation, what is current definition?"
- [x] **ナビゲーター調査** — current definition = QfnQ& とサーバーが回答
- [x] **鍵文字列の検証** — QfnQ& (コード [5,0,17,5,3]) → **正解確認済み**（Step3で鍵エコー、Step4で画像出力）
- [x] 正しい鍵で画像出力 — Step 4: p1=1, p2=2（画像データ取得成功）
- [x] **OOM対策** — mark-sweep GC + checkpoint/restore機構実装
- [ ] **画像レンダリング** — checkpoint/restoreベースのpixel-by-pixelレンダラーで実行中（Phase1 depth7進行中）
- [ ] 最終回答導出

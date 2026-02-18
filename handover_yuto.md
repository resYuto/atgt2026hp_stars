# handover_yuto

## 目的

- `ski_eval_rs` のOOMを抑えつつ、画像レンダリングを安定完走させる
- 最終的に画像から回答を読める状態を作る

reference: `reference/syugasato/deep-research-report.md`

## 進捗チェックリスト

### 0. ベースライン計測

- [ ] `--decode io --key 5,0,17,5,3` の基準コマンドを固定
- [ ] 3回実行して `max arena.nodes.len()` と所要時間のぶれを記録
- [ ] `GC回数 / free_list最小値 / 深さごとの出力ファイル` を記録

### 1. 低リスク最適化（定数再利用）

- [x] `decode_bool` の marker ノード再利用
- [x] `pair_fst/snd`, `pair1_fst/snd` の `K/I/KI` 再利用
- [x] diamond selector の再利用 (`build_diamond_sel`)
- [ ] `decode_church_num` の marker 再利用
- [ ] 上記適用後のメモリ増加率比較（ベースライン比）

### 2. `whnf` / `gc` の scratch再利用

- [ ] `whnf` の spine `Vec` を Arenaフィールドに移して再利用
- [ ] `gc` の markビット配列を Arenaフィールド化して再利用
- [ ] `gc` の DFS stack を Arenaフィールド化して再利用
- [ ] 同条件でピークメモリとGC時間を再計測

### 3. 画像レンダリング運用の固定化

- [ ] 深さ9〜25は中央ズーム + 128x128固定に統一
- [ ] `io` 経路で深さ9〜25を完走（OOMなし）
- [ ] 生成画像のうち有意なものを `png/` に保存
- [ ] 深さごとの出力ログ（黒/白/灰ピクセル数）を残す

### 4. `keyfind` のメモリ対策（必要時）

- [ ] `arena.nodes.clone()` ベース復元を checkpoint/restore 方式へ置換
- [ ] `keyfind` 実行時のメモリピーク比較

### 5. 品質ゲート

- [ ] `cargo build --release` 警告のうち未使用 `alloc` を削除
- [ ] `selftest` を通し、I/Oタグ解釈が不変であることを確認
- [ ] 画像生成結果が既知の進捗（depth 10以降の非自明パターン）と矛盾しないことを確認

### 6. 高リスク項目（後回し）

- [ ] packed node表現のPoCを別ブランチで実施
- [ ] compacting GCのPoCを別ブランチで実施
- [ ] 効果が十分な場合のみ本流へ取り込み

## 実行コマンド（基準）

```bash
cd ski_eval_rs
cargo build --release
./target/release/ski-eval ../very_large_txt/stars_compact.txt \
  --fuel 2000000000 --decode io --key 5,0,17,5,3 --img ../images/zoom
```

## 直近の優先順

1. フェーズ0（計測固定）
2. フェーズ2（spine/GC scratch再利用）
3. フェーズ3（深さ9〜25の運用固定）

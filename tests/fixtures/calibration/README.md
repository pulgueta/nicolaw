# Calibration pairs

`seed.tsv` holds labeled Spanish pairs of a claim and an evidence passage. They are the start of the calibration set that the main README describes (section "Calibration"). The pairs are synthetic. They come from the facts of the other test fixtures. They contain no private case material.

## Format

- UTF-8 text, one pair on each line, four fields separated by one tab character.
- The first line is the header: `claim\tpassage\tlabel\tfamily` (`\t` is a tab).
- `claim`: one standalone Spanish sentence, as the claim extraction writes it.
- `passage`: the evidence passage, as the NLI model and the decision step read it.
- `label`: the label of a reviewer, from the passage only: `contradicts`, `consistent`, `unrelated`, or `insufficient`.
- `family`: the fact family. All the pairs of one family (paraphrases, OCR and file variants of the same fact) go into the same split, tuning or locked test.
- A field cannot contain a tab or a line break.

## Seed

The seed has 24 pairs: 6 families, and one pair of each label in each family. It is too small to tune or to test thresholds. It only shows the format and runs the report:

```sh
cargo test real_calibration_pairs_report -- --ignored --nocapture
```

The report prints the scores of each pair and a confusion table of the decision step. It does not check accuracy.

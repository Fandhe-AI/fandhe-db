//! 変更ログのオラクル試験（D1 の層 3。RECOVER-14 のポインタ。設計は `docs/design/change-log-pitr.md`）。
//!
//! # 役割と責務境界
//!
//! 計装漏れ（捕捉を通らない書き込み）を、engine が記録した書き込みではなく **redb の実際の内容**から
//! 検出する。[`super::CaptureSession::prepare`] がレコードを `change_log` 表へ追記した直後に
//! [`verify_record`] を呼び、書き込みトランザクションの内容と基底（直前の commit）の実差分
//! （`table_image::capture`）がレコードに含まれ、レコードの値が書き込みトランザクションの現在値と
//! 一致することを確かめる。レコードを基底へ適用した結果が commit 後の状態と一致することと同値である。
//! 不一致は呼び出し側が panic にする（テスト専用。製品ビルドには存在しない）。
//!
//! 本モジュールは `cfg(test)` と cargo feature `change-log-oracle` の下にだけ存在する。有効化の条件:
//!
//! - lib の単体テスト（`cfg(test)`）では、変更ログを有効にした commit で常に検証する
//! - feature `change-log-oracle` のビルドでは、環境変数 `FANDHE_CHANGE_LOG_ORACLE=1` のときだけ
//!   検証し、永続 `Storage` の open で変更ログを強制的に有効にする（`tests/` の結合テストを
//!   変更ログ有効で流すため。環境変数が無ければ挙動は変わらない）。`FANDHE_CHANGE_LOG_ORACLE=force` は
//!   強制有効化だけで検証しない（検証は O(DB サイズ) / commit なので、遅延を比べる試験にはこちらを使う）
//!
//! # 再生器
//!
//! [`apply_record`]・[`dump_writer`] は、レコード列を空の状態から順に適用した結果が最終状態と一致する
//! ことを確かめるテスト（LSN の連鎖・符号化・正規化の通し検証）が使う小さな適用器である。
//! 後続の復元（#2169）の再生器の参照実装も兼ねる。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use super::{decode_record, is_excluded_table, ChangeRecord, RecordBody};
use crate::storage::table_image::{self, Capture, Meter, ReadMode, ReadOutcome};

/// 環境変数名（feature `change-log-oracle` のビルドでだけ参照する）。値は 2 通り:
/// `1` は強制有効化に加えて各 commit のレコードを実差分と突き合わせる（O(DB サイズ) / commit）、
/// `force` は強制有効化だけを行い検証はしない（有効時の実コストを測る・遅延の試験を流すため）。
pub(crate) const ENV_VAR: &str = "FANDHE_CHANGE_LOG_ORACLE";

fn env_mode() -> Option<&'static str> {
    static MODE: LazyLock<Option<&'static str>> =
        LazyLock::new(|| match std::env::var(ENV_VAR).as_deref() {
            Ok("1") => Some("1"),
            Ok("force") => Some("force"),
            _ => None,
        });
    *MODE
}

/// prepare 段でレコードを検証するか。
pub(super) fn active() -> bool {
    cfg!(test) || env_mode() == Some("1")
}

/// 永続 `Storage` の open で変更ログを強制的に有効にするか（feature ビルドで環境変数が `1` か `force`）。
pub(crate) fn forced() -> bool {
    cfg!(feature = "change-log-oracle") && env_mode().is_some()
}

/// 1 表の全行（キーのバイト列 → 値のバイト列）。
type Rows = BTreeMap<Vec<u8>, Vec<u8>>;

/// 表名 → (shape 番号, 全行)。
pub(crate) type Model = BTreeMap<String, (u32, Rows)>;

fn big_meter() -> Meter {
    Meter::new(usize::MAX, None)
}

fn read_full(write_txn: &redb::WriteTransaction, name: &str) -> Result<(u32, Rows), String> {
    match table_image::read_after_image(write_txn, name, ReadMode::All, &mut big_meter()) {
        Ok(ReadOutcome::Image(img)) => {
            let shape = u32::try_from(img.shape).map_err(|_| "shape overflow".to_string())?;
            Ok((shape, img.upserts.into_iter().collect()))
        }
        Ok(_) | Err(_) => Err(format!("cannot read table {name}")),
    }
}

/// writer の全表（変更ログ自身を除く）の内容を取る。
pub(crate) fn dump_writer(write_txn: &redb::WriteTransaction) -> Result<Model, String> {
    let names: Vec<String> = write_txn
        .list_tables()
        .map_err(|e| e.to_string())?
        .map(|h| redb::TableHandle::name(&h).to_string())
        .filter(|n| !is_excluded_table(n))
        .collect();
    let mut out = Model::new();
    for n in names {
        let t = read_full(write_txn, &n)?;
        out.insert(n, t);
    }
    Ok(out)
}

/// レコード本体を `model` へ適用する（`dropped` の削除 → `created` の作成 → `tables` の upsert／remove）。
pub(crate) fn apply_record(model: &mut Model, body: &RecordBody) -> Result<(), String> {
    for n in &body.dropped {
        if model.remove(n).is_none() {
            return Err(format!("dropped table {n} does not exist"));
        }
    }
    for (n, shape) in &body.created {
        if model.insert(n.clone(), (*shape, BTreeMap::new())).is_some() {
            return Err(format!("created table {n} already exists"));
        }
    }
    for t in &body.tables {
        let Some((shape, rows)) = model.get_mut(&t.name) else {
            return Err(format!("changed table {} does not exist", t.name));
        };
        if *shape != t.shape {
            return Err(format!("shape mismatch for table {}", t.name));
        }
        for (k, v) in &t.upserts {
            rows.insert(k.clone(), v.clone());
        }
        for k in &t.removes {
            rows.remove(k);
        }
    }
    Ok(())
}

/// 空の状態から `records`（LSN 昇順）を順に適用して最終状態を作る。LSN の連鎖
/// （`prev_lsn == 直前の lsn`、先頭は `start_lsn` の次）も検査する。
pub(crate) fn replay(
    start: Model,
    start_lsn: u64,
    records: &[ChangeRecord],
) -> Result<Model, String> {
    let mut model = start;
    let mut prev = start_lsn;
    for r in records {
        if r.header.prev_lsn != prev || r.header.lsn != prev + 1 {
            return Err("LSN chain is broken".to_string());
        }
        apply_record(&mut model, &r.body)?;
        prev = r.header.lsn;
    }
    Ok(model)
}

/// 2 つの状態の差を人が読める短い文字列にする（なければ `None`）。
pub(crate) fn diff_models(expected: &Model, actual: &Model) -> Option<String> {
    for (n, (shape, rows)) in expected {
        match actual.get(n) {
            None => return Some(format!("table {n} missing from replay")),
            Some((s2, r2)) => {
                if s2 != shape {
                    return Some(format!("table {n}: shape differs"));
                }
                if rows != r2 {
                    let missing = rows.keys().filter(|k| !r2.contains_key(*k)).count();
                    let extra = r2.keys().filter(|k| !rows.contains_key(*k)).count();
                    let changed = rows
                        .iter()
                        .filter(|(k, v)| r2.get(*k).is_some_and(|w| w != *v))
                        .count();
                    return Some(format!(
                        "table {n}: {missing} rows missing, {extra} extra, {changed} changed in replay"
                    ));
                }
            }
        }
    }
    actual
        .keys()
        .find(|n| !expected.contains_key(*n))
        .map(|n| format!("table {n} extra in replay"))
}

/// `bytes`（`change_log` 表へ追記したレコード）が、`base`（直前の commit）から `write_txn` の現在の内容
/// への実差分を過不足なく表していることを確かめる。`Err` は不一致の説明（テスト専用のため表名を含む）。
pub(super) fn verify_record(
    base: &redb::ReadTransaction,
    write_txn: &redb::WriteTransaction,
    bytes: &[u8],
) -> Result<(), String> {
    let rec = decode_record(bytes, usize::MAX).map_err(|e| format!("decode: {e:?}"))?;
    let base_gen = crate::storage::current_generation_in_txn(base).map_err(|e| e.to_string())?;
    if rec.header.prev_lsn != base_gen || rec.header.lsn != base_gen + 1 {
        return Err("record LSN does not follow the base generation".to_string());
    }

    let names = |it: Vec<String>| -> BTreeSet<String> {
        it.into_iter().filter(|n| !is_excluded_table(n)).collect()
    };
    let base_names = names(
        base.list_tables()
            .map_err(|e| e.to_string())?
            .map(|h| redb::TableHandle::name(&h).to_string())
            .collect(),
    );
    let final_names = names(
        write_txn
            .list_tables()
            .map_err(|e| e.to_string())?
            .map(|h| redb::TableHandle::name(&h).to_string())
            .collect(),
    );
    let dropped: BTreeSet<&String> = rec.body.dropped.iter().collect();
    let created: BTreeSet<&String> = rec.body.created.iter().map(|(n, _)| n).collect();
    for n in &dropped {
        if !base_names.contains(*n) {
            return Err(format!("dropped table {n} is not in the base"));
        }
    }
    for n in &created {
        if !final_names.contains(*n) {
            return Err(format!("created table {n} is not in the final state"));
        }
    }
    for n in final_names.difference(&base_names) {
        if !created.contains(n) {
            return Err(format!("new table {n} is not recorded as created"));
        }
    }
    for n in base_names.difference(&final_names) {
        if !dropped.contains(n) {
            return Err(format!("removed table {n} is not recorded as dropped"));
        }
    }
    for n in &created {
        if base_names.contains(*n) && !dropped.contains(*n) {
            return Err(format!("recreated table {n} is not recorded as dropped"));
        }
    }
    for n in &dropped {
        if final_names.contains(*n) && !created.contains(*n) {
            return Err(format!(
                "dropped table {n} still exists but is not recreated"
            ));
        }
    }

    // 作成（または再作成）された表は、全行が書き込みトランザクションの内容と一致する。
    for (n, shape) in &rec.body.created {
        let (wshape, rows) = read_full(write_txn, n)?;
        if wshape != *shape {
            return Err(format!("created table {n}: shape differs"));
        }
        let change = rec.body.tables.iter().find(|t| &t.name == n);
        let recorded: BTreeMap<&Vec<u8>, &Vec<u8>> = change
            .map(|c| c.upserts.iter().map(|(k, v)| (k, v)).collect())
            .unwrap_or_default();
        if change.is_some_and(|c| !c.removes.is_empty()) {
            return Err(format!("created table {n}: unexpected removes"));
        }
        let expected: BTreeMap<&Vec<u8>, &Vec<u8>> = rows.iter().collect();
        if recorded != expected {
            return Err(format!(
                "created table {n}: rows differ from the final state"
            ));
        }
    }

    // 実差分（基底 → 書き込みトランザクション）の網羅: 差分の upsert・remove がレコードに載っている。
    let diff = match table_image::capture(write_txn, base, &mut big_meter()) {
        Ok(Capture::Image(img)) => img,
        Ok(Capture::Gate) => return Err("oracle cannot capture the diff (gate)".to_string()),
        Err(_) => return Err("oracle capture failed".to_string()),
    };
    for delta in diff.oracle_deltas() {
        let (name, shape, upserts, removes) =
            (delta.name, delta.shape, delta.upserts, delta.removes);
        if created.contains(&name.to_string()) {
            continue;
        }
        let Some(change) = rec.body.tables.iter().find(|t| t.name == name) else {
            return Err(format!("table {name}: changed but not recorded"));
        };
        if usize::try_from(change.shape).ok() != Some(shape) {
            return Err(format!("table {name}: shape differs"));
        }
        let recorded: BTreeMap<&Vec<u8>, &Vec<u8>> =
            change.upserts.iter().map(|(k, v)| (k, v)).collect();
        for (k, v) in upserts {
            if recorded.get(k) != Some(&v) {
                return Err(format!("table {name}: an upsert is missing or differs"));
            }
        }
        let recorded_removes: BTreeSet<&Vec<u8>> = change.removes.iter().collect();
        for k in removes {
            if !recorded_removes.contains(k) {
                return Err(format!("table {name}: a removal is not recorded"));
            }
        }
    }

    // 記録した値が書き込みトランザクションの現在値と一致する（余分な記録は no-op に限る）。
    for change in &rec.body.tables {
        if created.contains(&change.name) {
            continue;
        }
        if !final_names.contains(&change.name) {
            return Err(format!("table {}: recorded but absent", change.name));
        }
        let keys: BTreeSet<Vec<u8>> = change
            .upserts
            .iter()
            .map(|(k, _)| k.clone())
            .chain(change.removes.iter().cloned())
            .collect();
        let img = match table_image::read_after_image(
            write_txn,
            &change.name,
            ReadMode::Keys(&keys),
            &mut big_meter(),
        ) {
            Ok(ReadOutcome::Image(img)) => img,
            _ => return Err(format!("table {}: cannot read back", change.name)),
        };
        let now_up: BTreeMap<&Vec<u8>, &Vec<u8>> =
            img.upserts.iter().map(|(k, v)| (k, v)).collect();
        let now_rm: BTreeSet<&Vec<u8>> = img.removes.iter().collect();
        for (k, v) in &change.upserts {
            if now_up.get(k) != Some(&v) {
                return Err(format!("table {}: a recorded value is stale", change.name));
            }
        }
        for k in &change.removes {
            if !now_rm.contains(k) {
                return Err(format!(
                    "table {}: a recorded removal is stale",
                    change.name
                ));
            }
        }
    }
    Ok(())
}

# shellcheck shell=bash
# $EPOCHREALTIME をロケールに依存せずマイクロ秒の整数へ変換するヘルパー（source 専用）。
#
# 役割: crash_test_pitr.sh（Issue #3138・RECOVER-14 ポインタ）が壁時計を採るときに使う。
# 背景: bash は $EPOCHREALTIME の小数点にロケールの文字を使う。小数点がカンマのロケールでは
#   小数点を 1 文字消す置換ではカンマを消せず、算術式がカンマ演算子として評価されて黙って誤った値になる。
# 方針: 区切りを [.,] に限定した正規表現で解析し、形式外は return 1（fail-closed）にする。
#   $(...) を使わないため fork せず、/proc/uptime との連続採取の性質を保てる。
# set -e 等のシェル設定は変えない（呼び出し元が kill/wait の終了コードのため set -e を避けている）。

# parse_epoch_us <文字列>: 成功時にグローバル変数 EPOCH_US へマイクロ秒を入れる。形式外は 1 を返す。
parse_epoch_us() {
    [[ "${1-}" =~ ^([0-9]+)[.,]([0-9]{6})$ ]] || return 1
    # shellcheck disable=SC2034  # 呼び出し元（source 側）が読むグローバル変数
    EPOCH_US=$((10#${BASH_REMATCH[1]}${BASH_REMATCH[2]}))
}

# read_epoch_us: 現在の $EPOCHREALTIME を parse_epoch_us へ渡す。
read_epoch_us() {
    parse_epoch_us "${EPOCHREALTIME-}"
}

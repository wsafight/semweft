#!/bin/sh
set -u

semweft_bin=${SEMWEFT_BIN:-semweft}
semweft_diff=${SEMWEFT_DIFF:-HEAD~1..HEAD}
semweft_reports=${SEMWEFT_REPORT_DIR:-semweft-reports}
mkdir -p "$semweft_reports" || exit 5

"$semweft_bin" verify --diff "$semweft_diff" --all-pages --format json > "$semweft_reports/verify.json"
semweft_status=$?
printf '%s\n' "$semweft_status" > "$semweft_reports/exit-code.txt"
case "$semweft_status" in
    0|1|2|3|4|5) ;;
    *) exit "$semweft_status" ;;
esac

for semweft_format in markdown sarif; do
    "$semweft_bin" report --input "$semweft_reports/verify.json" --format "$semweft_format" > "$semweft_reports/verify.$semweft_format"
    semweft_render_status=$?
    if [ "$semweft_render_status" -ne "$semweft_status" ]; then
        exit "$semweft_render_status"
    fi
done

if [ "$semweft_status" -eq 3 ] && [ "${SEMWEFT_ALLOW_INSUFFICIENT:-0}" = 1 ]; then
    exit 0
fi
exit "$semweft_status"

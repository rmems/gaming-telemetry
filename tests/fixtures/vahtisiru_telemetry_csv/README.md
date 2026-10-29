# Vendored vahtisiru telemetry CSV fixtures

Byte-for-byte copies of `rmems/vahtisiru` `tests/fixtures/telemetry_csv/*.csv`
at commit `4feb196b53509461ceb9b1e7f9a6dd35e67db280` (RM-1904, #82). The
normative semantics and the expected result for each file are in the upstream
`tests/fixtures/telemetry_csv/README.md` and `docs/telemetry_csv.md`.

Do not edit these files here. Re-sync them from upstream deliberately and bump
`gaming_telemetry::corinth_csv::CONTRACT_REVISION` in the same change;
`tests/corinth_csv_contract.rs` checks this copy against those expectations.

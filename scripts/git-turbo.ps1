param([Parameter(ValueFromRemainingArguments = $true)][string[]]$GitArgs)
# Per-invocation setting; does not change the user's global Git configuration.
& git -c 'http.https://github.com/.proxy=http://127.0.0.1:18080' @GitArgs
exit $LASTEXITCODE

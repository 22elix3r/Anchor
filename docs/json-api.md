# JSON and exit-status contract

Commands that accept `--format json` emit one JSON object to standard output:

```json
{
  "schema": 1,
  "operation": "diff",
  "status": "ok",
  "data": {}
}
```

- `schema` versions the complete envelope and command payload contract.
- `operation` identifies the command independently from localized human text.
- `status` is a stable machine-readable outcome for the operation.
- `data` is always present. It is an object, array, or `null` depending on the
  command.

Schema 1 covers `status`, `sessions`, `deleted-sessions`, `show`, `diff`,
`restore`, `rollback`, `restore-index`, `doctor`, `gc`, `recover`, and
`recover-transactions`. Errors detected before an operation can construct its
result are diagnostics on standard error and may not have a JSON envelope.
Consumers must check the process exit status as well as the JSON object.

Common status values include `ok`, `empty`, `nonterminal`, `differences`,
`no-differences`, `preview`, `confirmation-required`, `applied`, `no-change`,
`conflict`, and `attention`. Operations use only the subset relevant to their
state machine.

## Exit statuses

| Status | Meaning |
|---:|---|
| 0 | Operation succeeded or no mutation was needed |
| 1 | `diff` found differences, `doctor` found a condition needing attention, or an ordinary command error occurred |
| 3 | Session remains nonterminal, or a restore/rollback preview requires a bound confirmation |
| 4 | Restore or index conflict; current state was preserved |
| Child status | `run` returns the wrapped child's exit status after attempting session-end capture |
| `128 + signal` | Unix `run` child ended from a signal |

Do not infer success from a payload field alone. A future incompatible JSON
contract increments `schema`; it does not silently repurpose schema-1 fields.

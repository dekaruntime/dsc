# dsc#245 / dsc#246 — multi-line JSX return parens, `<React.Fragment>`

dsc#245 requires a multi-line `return` of a bare JSX expression to be wrapped
in parentheses, matching TS/React (single-line returns are unaffected).
dsc#246 allows `<React.Fragment>` as the one other RFD 8 member-tag exception
alongside `<Ctx.Provider>`, so a keyed fragment has an expression.

## Reviewed bases

| Owner | Pin | SHA-256 |
| --- | --- | --- |
| testsuite | `corpus-v0.53.0` | `44c7087d13240c79cfbcae6ea3e8cc6cd0eaf8dc9214a53e1c8f2a0d045bb714` |
| tour | `60c4add32df11536ba2856e43d6ac07a263eb788` | `373590eddea2eec5af222fd5ff49cdd2dfc7359b73b3a30cc2967fd82dd0839e` |

Apply `tour.patch` at the tour repository root. The pin is unchanged here.

## Corpus migration

None. Swept every `.pass.ds` / `.pass.dsx` fixture in `corpus-v0.53.0` with
`dsc check --single-file` after landing the parser rule: zero fixtures hit
the new diagnostic. No `testsuite.patch` is included.

## Tour migration

Three lessons use the bare multi-line form and now need parens:

- `utility-classes.dsx`
- `props.dsx`
- `props-checked.dsx`

Nothing in the tour lesson set uses `<React.Fragment>` or any other RFD 8
member tag beyond `.Provider`, so dsc#246 needs no tour change.

## Deka scaffold (separate repo, not covered by this migration)

`crates/pm/scaffold/app/page.dsx` in `dekaruntime/deka` also uses the bare
multi-line form. That repo is out of dsc's reach from here — filed as a
follow-up for the deka owner (Khalid) rather than patched in this migration.

# TODO: suporte a SQL Server (trabalho em andamento)

Já feito nesta branch: o driver `crates/driver-sqlserver` (tiberius 0.13), as
variantes `DriverKind::SqlServer` e `SqlDialect::SqlServer`, o registro do
driver, as regras de T-SQL em `jykre-sql` (`[x]`, `N''`, `@var`, `#temp`,
`GO`, blocos e rotinas, explain via SHOWPLAN), o formatter `tsql`, o
catálogo `catalog/sqlserver`, os serviços 2019/2022 no
`docker-compose.test.yml` e o seed do gate.

Testado: unitários e scaffold do driver, `jykre-sql`, `jykre-sql-language`
(com e sem `sqruff`) e xtask. A suíte live do driver passou só no SQL Server
2022, rodada à mão.

## Falta

- [ ] **App (`src/`)**
  - `transaction.rs`: regras T-SQL (`BEGIN TRAN`, `COMMIT/ROLLBACK [TRAN]`, `SAVE TRAN`).
  - `copy_as.rs` e `export.rs`: bit como `1`/`0` e strings `N'…'`.
  - `cell_format::family`: `DATETIME2`, `DATETIMEOFFSET`, `SMALLDATETIME`, `SMALLMONEY`, `IMAGE`.
  - Conferir que o Explain em `workspace.rs` envia o texto como uma única requisição.
- [ ] **Diálogo de conexão** (`src/app/connections.rs`, `src/app/mod.rs`)
  - Índice 3 = SQL Server; porta 1433.
  - Select de `encrypt` (`off`/`mandatory`/`strict`) e checkbox `trustServerCertificate`.
  - Cor em `connection_shade` e texto de boas-vindas.
  - Strings novas em `locales/pt-BR.json`.
- [ ] **Testes**
  - Braço `sqlserver` em `tests/live.rs` (export de 250 mil linhas).
  - Teste de `options(&["sqlserver"])` no xtask.
  - Serviço em `docker-compose.dev.yml` e seed em `dev/seed`.
- [ ] **Docs**
  - Novo ADR-0025 (tiberius, custo do aws-lc-rs).
  - Atualizar `stack-and-dependencies.md`, `drivers.md`, `architecture.md`, `testing.md`, `especificacao-catalogo-sql.md`, `README.md` e `docs/README.md`.
- [ ] **Desvios conhecidos e débito técnico TD-009** (a sessão já cita TD-009)
  - Cancelamento ou timeout fecha a sessão: o tiberius não lê o ack do attention, que o servidor manda em mensagem separada.
  - `money` acima de cerca de 450 bilhões dá erro, nunca valor aproximado.
  - Só o primeiro resultado de um batch é mostrado.
  - O schema da aba não pode ser aplicado.
  - Comandos com parâmetros rodam dentro de `sp_executesql`.
- [ ] **Gate**: `cargo xtask check`, `cargo xtask check --full` (live no 2019 e no 2022), `cargo deny --locked check`, verificação visual do diálogo e dos fluxos.
- [ ] Remover este arquivo quando tudo estiver concluído.

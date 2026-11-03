# Pendências da auditoria comportamental

Comparação com o comportamento consolidado em gerenciadores profissionais de banco de dados: o que a aplicação **já faz**, mas de modo pior, inconsistente ou imprevisível. Este arquivo lista **somente o que ainda falta** depois do `git pull` em `release/0.1.0` (`876fdde`, 3 de novembro de 2026).

Números `Nxxx` são os da auditoria original, para rastreio. Achados fechados pelos commits recentes não se repetem abaixo.

## Resumo

O pull em `release/0.1.0` trouxe três commits (`b6268bf`, `355bbe3`, `876fdde`). O núcleo do fluxo diário de execução mudou de fato:

| Situação | Quantidade | IDs |
| --- | ---: | --- |
| Fechados | 9 | N001, N003, N004, N007, N008, N009, N016, N017, N020 |
| Parciais (restam trechos) | 4 | N005, N006, N018, N035 |
| Ainda abertos | 28 | N002, N010–N015, N019, N021–N034, N036–N041 |
| **Pendências neste arquivo** | **32** | os parciais + os abertos |

**O que os commits fecharam**

- **N001.** `Ctrl+Enter` executa a seleção ou a instrução no cursor; `Ctrl+Shift+Enter` abre outro result tab; `Ctrl+Alt+Enter` roda o script.
- **N003 / N004 / N008 / N009.** Cada statement ganha um result tab; falha não esconde os anteriores; Messages lista cada um; DML mostra *rows affected*; `Ctrl+Enter` substitui o último tab em vez de apagar tudo.
- **N007.** Arrastar aba ou trocar conexão na toolbar pergunta antes; o SQL permanece.
- **N016.** O result tab guarda o SQL executado, separado do editor.
- **N017.** Abas, SQL, conexão, timeout, consultas salvas e os últimos 200 itens de histórico ficam em `config.db` (`workspace.state`).
- **N020.** Sessão por aba entre runs (ADR-0014): `USE`, `SET` e temporárias sobrevivem. Databricks acompanha `USE CATALOG` / `USE SCHEMA`.

**O que ainda pesa no dia a dia**

Os dois críticos originais que restam: erros do servidor continuam genéricos (**N002**) e o splitter ainda ignora o dialeto (**N010**). Em seguida: format que pode alterar literais (**N011**), credenciais e carga de conexões (**N012–N014**), novo run durante cancelamento (**N015**), teclado que não ativa botões (**N019**) e o lote marcado todo como Running (**N021**).

Transação explícita, fechar aba com SQL não salvo, colisão de título na biblioteca e histórico sem falhas foram só parcialmente cobertos.

A aplicação não foi executada nesta conferência; o estado abaixo foi lido no código em `876fdde`.

---

## N002 — Erros do servidor são descartados e substituídos por texto genérico

- **Prioridade:** Crítica
- **Funcionalidade:** Diagnóstico / drivers / UI
- **Comportamento atual:** `map_error` do PostgreSQL e do MySQL classifica o `ErrorCode` e grava `"Database operation failed; check connectivity, permissions and SQL"`. `QueryRun::finish` guarda `error.to_string()`. A área de resultados e o teste de conexão mostram só essa frase. Sem título por código, sem SQLSTATE, sem posição, sem Copy details. `docs/ux.md` e `docs/reliability.md` ainda registram o desvio.
- **Comportamento de referência:** Gerenciadores profissionais mostram a mensagem do servidor, o código e, quando há, marcam a posição no editor.
- **Diferença identificada:** A classificação existe internamente e morre na fronteira com o usuário.
- **Por que o comportamento atual é inferior:** Sintaxe, constraint, objeto inexistente, senha e timeout ficam indistinguíveis.
- **Comportamento recomendado:** Preservar metadados seguros do driver (`code`, mensagem do servidor, posição) e renderizar título + mensagem + ação + detalhes copiáveis.
- **Evidência no projeto:** `crates/driver-postgres/src/runtime.rs` (`map_error`); `crates/driver-mysql/src/runtime.rs`; `src/query.rs` (`finish`); `src/app/workspace.rs` (`ResultState::Failed`); `src/app/connections.rs` (`test_connection`)
- **Confiança:** Confirmado
- **Situação:** Corrigido. `DriverError` carrega `ServerDiagnostic` (SQLSTATE, código do fornecedor, posição) e a mensagem do servidor; PostgreSQL inclui detail/hint, MySQL usa o número do erro (1045, 1142…) e Databricks repassa `status.error`. `src/error_report.rs` dá título por código/classe SQLSTATE, ação sugerida e **Copy details**; o result tab mostra tudo, põe o cursor na posição (sem selecionar) e oferece “Go to the position”, que também foca o editor. O teste de conexão mostra título, mensagem e ação.

## N005 — Mensagem transacional e transação explícita invisível

- **Prioridade:** Alta
- **Funcionalidade:** Transações / DML
- **Comportamento atual:** A sessão da aba agora é mantida (N020 fechado). Só autocommit é suportado como fluxo. Após falha parcial a UI ainda afirma: “Previous statements were not rolled back automatically.” Um `BEGIN` explícito fica aberto na sessão, sem indicador, sem Commit/Rollback, e fechar a aba faz rollback sem perguntar. Dívida técnica em ADR-0014 e `docs/architecture.md`.
- **Comportamento de referência:** Gerenciadores profissionais expõem o modo da transação (auto-commit ou manual) e o que fazer no erro do script (parar e reverter, parar e confirmar, ou ignorar).
- **Diferença identificada:** No autocommit a frase ficou mais próxima da verdade; no `BEGIN` explícito ela pode estar errada, e o usuário não vê que há transação aberta.
- **Por que o comportamento atual é inferior:** Fechar a aba (ou perder a sessão) pode reverter trabalho que a mensagem descreveu como já persistido — ou o contrário.
- **Comportamento recomendado:** Distinguir autocommit de transação aberta; mostrar estado; confirmar ao fechar com transação aberta; não afirmar rollback que o runtime não observou.
- **Evidência no projeto:** `src/query.rs` (`finish`); `docs/adr/0014-a-session-per-tab.md`; `docs/architecture.md` (Known deviations)
- **Confiança:** Confirmado
- **Nota:** Parcial — a sessão deixou de ser descartada a cada run; falta o fluxo transacional visível.

## N006 — Fechar aba descarta SQL sem confirmação e é inacessível pelo teclado

- **Prioridade:** Alta
- **Funcionalidade:** Abas
- **Comportamento atual:** Fechar com query **em execução** agora pergunta (“Close and cancel”). Sem execução, o X remove a aba na hora: o SQL daquela aba some do `workspace.state` na próxima gravação. Não há dirty flag para SQL não salvo na biblioteca. O botão Close tem `tab_stop(false)` e não existe `Ctrl+W` / `Ctrl+F4`.
- **Comportamento de referência:** Gerenciadores profissionais autossalvam o editor, recuperam consoles fechados e pedem confirmação para scripts alterados.
- **Diferença identificada:** A guarda cobre só o cancelamento do job, não a perda do texto da aba nem o teclado.
- **Por que o comportamento atual é inferior:** Clique acidental no X ainda perde SQL que não estava na biblioteca. Usuário de teclado não fecha a aba.
- **Comportamento recomendado:** Confirmar SQL alterado e não salvo; atalho de fechar; X alcançável pelo teclado.
- **Evidência no projeto:** `src/app/mod.rs` (`request_close`); `src/grouped_tabs/mod.rs` (botão Close, `tab_stop(false)`); `src/workspace_state.rs`
- **Confiança:** Confirmado
- **Nota:** Parcial — confirmação para query rodando foi implementada.
- **Situação:** Corrigido. Fechar pergunta também quando o SQL não está na biblioteca e difere do que a aba abriu (`TabState::has_unsaved_sql`; o SQL de abertura persiste como `opened_sql`). `Ctrl+W` e `Ctrl+F4` são a action `CloseActiveTab`; o X da aba selecionada fica sempre visível e é o próximo Tab stop, e o foco volta para a aba que fica selecionada. `Ctrl+N` agora foca o editor da aba nova (antes o foco ficava num editor fora da tela e nenhum atalho respondia).

## N010 — Separação de instruções ignora o dialeto

- **Prioridade:** Alta
- **Funcionalidade:** SQL enviado
- **Comportamento atual:** Todo `;` fora de aspas/dollar-quote termina statement. `/* */` inicial é descartado — inclusive `/*! ... */` do MySQL. Sem `DELIMITER`. O commit `876fdde` registrou isso como dívida técnica: `CREATE PROCEDURE`/`TRIGGER` MySQL e `BEGIN ATOMIC` PostgreSQL continuam fatiados.
- **Comportamento de referência:** Gerenciadores profissionais separam instruções com parser por dialeto, delimiter nativo e, quando configurado, linha em branco.
- **Diferença identificada:** O splitter é genérico num produto que já tem três dialetos. Com `Ctrl+Enter` no cursor isso afeta o que é escolhido como “a instrução”.
- **Por que o comportamento atual é inferior:** Corpo de procedure MySQL e comentários executáveis podem nunca chegar íntegros ao servidor.
- **Comportamento recomendado:** Splitter por dialeto, com rotinas, `DELIMITER` e comentários executáveis.
- **Evidência no projeto:** `src/sql.rs` (`statements`); `docs/architecture.md` (Known deviations)
- **Confiança:** Confirmado

## N011 — Format pode alterar strings MySQL e sempre perde cursor/seleção

- **Prioridade:** Alta
- **Funcionalidade:** Formatação
- **Comportamento atual:** `tokens()` (usado por `format`) não trata `\\` em strings; o splitter trata. `replace_all` da API GPUI reposiciona documento multiline em `0..0` e zera o scroll.
- **Comportamento de referência:** Gerenciadores profissionais formatam por dialeto e preservam o cursor na região reformatada.
- **Diferença identificada:** Dois tokenizers discordam; o format reescreve o buffer e joga o usuário ao início.
- **Por que o comportamento atual é inferior:** `'a\\'b'` no MySQL pode mudar de sentido; mesmo SQL válido perde o ponto de edição.
- **Comportamento recomendado:** Tokenizer por dialeto; restaurar caret, seleção e scroll.
- **Evidência no projeto:** `src/sql.rs` (`tokens` vs `statements`); `src/app/workspace.rs` (botão Format)
- **Confiança:** Confirmado no Jykre; Provável na preservação de caret das ferramentas de referência

## N012 — Senha antiga é reenviada ao trocar de driver

- **Prioridade:** Alta
- **Funcionalidade:** Autenticação
- **Comportamento atual:** Campo vazio reusa `editing_connection.input.credentials`, inclusive depois de mudar PostgreSQL → MySQL. Só Databricks zera.
- **Comportamento de referência:** Senha vazia no edit costuma significar “manter a mesma identidade”, não portar credencial para outro motor.
- **Diferença identificada:** A identidade de autenticação não é invalidada com o driver.
- **Por que o comportamento atual é inferior:** Credencial de outro servidor pode ir em silêncio ao destino novo.
- **Comportamento recomendado:** Preservar senha só se host/usuário/driver continuarem compatíveis; troca de driver exige senha explícita.
- **Evidência no projeto:** `src/app/connections.rs` (`form_connection`)
- **Confiança:** Confirmado
- **Situação:** Corrigido. `connection_store::keeps_password` só deixa a senha vazia manter a anterior quando driver, host, porta e usuário são os mesmos; senão a credencial não é enviada e, se estava lembrada, sai do cofre ao salvar. O diálogo avisa quando a senha anterior não será usada.

## N013 — “Remember password” permanece ativo sem senha no vault

- **Prioridade:** Alta
- **Funcionalidade:** Vault
- **Comportamento atual:** O perfil é salvo com `remember_password=true` **antes** de `secrets.set`. Falha no vault vira warning. Na leitura, `NoEntry` vira `Credentials::default()` sem aviso. Um erro de cache posterior pode sobrescrever o warning.
- **Comportamento de referência:** A preferência “lembrar senha” deve refletir o que realmente está armazenado.
- **Diferença identificada:** Intenção persistida ≠ estado do vault.
- **Por que o comportamento atual é inferior:** Na próxima abertura a UI mente, e o problema some visualmente.
- **Comportamento recomendado:** `remember_password=false` se a gravação falhar, ou estado “solicitado, indisponível”; acumular avisos.
- **Evidência no projeto:** `src/connection_store.rs` (`save_to`); `crates/secrets/src/lib.rs` (`get`)
- **Confiança:** Confirmado
- **Situação:** Corrigido. `save_to` grava o cofre antes do perfil e salva `remember_password=false` quando a senha não foi guardada (cofre indisponível ou nenhuma senha disponível); um perfil novo que falha ao salvar não deixa senha no cofre. Na leitura, `remember_password` sem entrada no cofre gera aviso. Avisos se acumulam (`add_warning`) em vez de se sobrescreverem, e o explorer não repete o mesmo aviso como status.

## N014 — Um perfil ou preferência inválidos escondem todas as conexões

- **Prioridade:** Alta
- **Funcionalidade:** Inicialização
- **Comportamento atual:** `load_from` aborta a lista no primeiro `validate_connection` ou `row_limit` que falha. `load_connections` ainda junta conexões, `editor.font_size` e `workspace.state` no mesmo `Result` — agora uma fonte inválida também impede restaurar as abas.
- **Comportamento de referência:** Falha de um data source não oculta os demais.
- **Diferença identificada:** Isolamento de falha prometido em `docs/reliability.md` não vale para a carga inicial.
- **Por que o comportamento atual é inferior:** Um JSON ruim ou uma fonte inválida deixa o explorer vazio e, com o novo snapshot, também pode impedir o restore do workspace.
- **Comportamento recomendado:** Carregar preferências e workspace à parte; isolar falhas por perfil.
- **Evidência no projeto:** `src/connection_store.rs` (`load_from`); `src/app/connections.rs` (`load_connections`)
- **Confiança:** Confirmado
- **Situação:** Corrigido. `ConfigRepository::list_each` lê cada perfil por si; `connection_store::load` devolve `LoadedConnections` com as conexões válidas e um problema por perfil ilegível ou inválido (um JSON legado inválido também vira problema e é preservado sem importar). Limite de linhas ilegível usa o padrão com aviso, e salvar não falha por causa de outro perfil. `load_connections` lê conexões, fonte e workspace separadamente e relata cada problema.

## N015 — É possível iniciar outra execução enquanto a anterior ainda cancela

- **Prioridade:** Alta
- **Funcionalidade:** Cancelamento / concorrência
- **Comportamento atual:** `start` rejeita só `Running`. Em `Cancelling`, suplanta o run (“A run that is only Cancelling is superseded”). O workspace abre **outra** sessão enquanto a antiga ainda para: “A superseded run still holds its session; this one opens another.”
- **Comportamento de referência:** Gerenciadores profissionais cancelam o job do console e avisam se o cancelamento se prolonga; não costumam iniciar um segundo job no mesmo console enquanto o primeiro ainda para.
- **Diferença identificada:** Cancelamento cooperativo + novo lote = duas sessões, uma invisível. Com sessão por aba isso ficou mais grave: a sessão antiga e a nova podem coexistir.
- **Por que o comportamento atual é inferior:** A query antiga ainda pode alterar dados; o desfecho some da UI.
- **Comportamento recomendado:** Bloquear Run até o cancelamento terminar, ou exibir os dois jobs.
- **Evidência no projeto:** `src/query.rs` (`start`); `src/app/workspace.rs` (`execute_queries`)
- **Confiança:** Confirmado no Jykre; Provável na comparação (as ferramentas de referência não documentam o caso explicitamente)
- **Situação:** Corrigido. `QueryRun::start` recusa qualquer run ativo, inclusive `Cancelling`; nenhuma segunda sessão é aberta enquanto a anterior para. Run, atalhos e gutter avisam “The previous run is still stopping…”; o botão mostra “Stopping…” e o aviso some quando o desfecho chega.

## N018 — Consultas salvas colidem por título

- **Prioridade:** Alta
- **Funcionalidade:** Biblioteca
- **Comportamento atual:** A lista **persiste** em `workspace.state` (a volatilidade original foi fechada). A identidade continua `(title, connection)`: `retain` apaga a homônima na mesma conexão. Duas “Active customers” na mesma conexão se sobrescrevem sem aviso.
- **Comportamento de referência:** Scripts salvos têm identidade estável, independente do nome exibido.
- **Diferença identificada:** Persistência existe; a chave de identidade não.
- **Por que o comportamento atual é inferior:** Salvar com o mesmo título substitui a outra consulta em silêncio, agora de forma durável.
- **Comportamento recomendado:** ID estável; nome separado da identidade.
- **Evidência no projeto:** `src/app/mod.rs` (`WorkspaceEvent::Saved`, `retain`); `src/workspace_state.rs` (`QueryEntry`)
- **Confiança:** Confirmado
- **Nota:** Parcial — persistência implementada; colisão permanece.

## N019 — Controles focáveis não implementam Enter/Espaço

- **Prioridade:** Alta
- **Funcionalidade:** Teclado
- **Comportamento atual:** `base_button` / `icon_button` / nós do explorer: `Role::Button`, `focusable`, `on_click`. Sem handler de Enter/Espaço. Só os três atalhos de run são actions GPUI.
- **Comportamento de referência:** Botão de desktop ativa com Enter/Espaço; árvore navega com setas.
- **Diferença identificada:** A affordance de foco mente.
- **Por que o comportamento atual é inferior:** Tab chega no Run e a tecla não dispara. Explorer exige Tab por item.
- **Comportamento recomendado:** Componente de botão que sintetiza Enter/Espaço; árvore com setas.
- **Evidência no projeto:** `src/app/ui.rs`; `src/app/connections.rs` (nós); `docs/ui-guidelines.md`
- **Confiança:** Confirmado
- **Situação:** Corrigido. Conferido no app (Xvfb): o GPUI usado (`gpui-pre` 0.3.11) já converte Enter/Espaço no elemento focado em `ClickEvent::Keyboard`, e Format/Run/Close respondem pelo teclado. O defeito real era o foco invisível: o Run (botão primário) tinha borda de foco menta sobre fundo menta e os botões de tamanho de fonte não tinham estilo de foco. Ambos ganharam foco visível, e `docs/ui-guidelines.md` registra a regra (`on_click` + `focusable`). Setas na árvore ficaram com a N026.

## N021 — Todas as instruções aparecem como Running antes de começarem

- **Prioridade:** Média
- **Funcionalidade:** Gutter
- **Comportamento atual:** `execute_queries` ainda chama `begin_all`, que marca o lote. O backend executa sequencialmente. O demo usa `begin_statement` um a um.
- **Comportamento de referência:** Pendente / executando / concluído por statement.
- **Diferença identificada:** O gutter mente simultaneidade no caminho real.
- **Por que o comportamento atual é inferior:** Não se vê qual statement longo está no servidor; qualquer Stop no gutter para o lote inteiro.
- **Comportamento recomendado:** Callbacks por instrução: Pending → Running → Success/Error.
- **Evidência no projeto:** `src/app/workspace.rs` (`begin_all`); `src/query.rs`; `src/backend.rs`
- **Confiança:** Confirmado

## N022 — Filtrar a grade apaga a seleção

- **Prioridade:** Média
- **Funcionalidade:** Grade
- **Comportamento atual:** `set_filter` chama `select(None)`. O modelo `CellSelection` foi feito para sobreviver a filtro/sort (`docs/results.md`).
- **Comportamento de referência:** Grades profissionais mantêm a célula se ela continua visível.
- **Diferença identificada:** O modelo permite preservar; a view descarta.
- **Por que o comportamento atual é inferior:** Cada tecla no filtro derruba a seleção.
- **Comportamento recomendado:** Preservar por índice de dados; ocultar só se os extremos sumirem.
- **Evidência no projeto:** `src/app/result_grid.rs` (`set_filter`); `src/grid.rs`; `docs/results.md`
- **Confiança:** Confirmado no Jykre; Provável na comparação

## N023 — Filtro trata NULL como o texto “NULL”

- **Prioridade:** Média
- **Funcionalidade:** Grade
- **Comportamento atual:** `cell.unwrap_or("NULL").to_lowercase().contains(filter)` — nulos reais batem com a busca `null`.
- **Comportamento de referência:** Gerenciadores profissionais distinguem filtro de nulo do texto.
- **Diferença identificada:** A apresentação de NULL vaza para a semântica da busca.
- **Por que o comportamento atual é inferior:** Mistura nulos com o literal `NULL`.
- **Comportamento recomendado:** Não indexar nulos no texto do filtro, ou ter critério explícito “is null”.
- **Evidência no projeto:** `src/grid.rs` (`GridView::new`)
- **Confiança:** Confirmado

## N024 — O botão Explain não executa EXPLAIN

- **Prioridade:** Média
- **Funcionalidade:** Explain
- **Comportamento atual:** Chama `ask("Explain this query")`, que devolve texto fixo do exemplo de vendas. Sem badge MOCK (o assistente tem “IA”; o Chart admite “only in the demo”).
- **Comportamento de referência:** Gerenciadores profissionais enviam `EXPLAIN` / `EXPLAIN ANALYZE` ao banco e mostram o plano real.
- **Diferença identificada:** Ação real da toolbar, significado simulado.
- **Por que o comportamento atual é inferior:** Em conexão real, o nome promete um plano que não existe.
- **Comportamento recomendado:** Renomear enquanto for mock, ou executar o EXPLAIN do driver.
- **Evidência no projeto:** `src/app/workspace.rs` (botão Explain, `ask`)
- **Confiança:** Confirmado

## N025 — “Insert into editor” substitui todo o SQL

- **Prioridade:** Média
- **Funcionalidade:** Assistente
- **Comportamento atual:** O botão chama `replace_all` sem diff nem confirmação.
- **Comportamento de referência:** Assistentes profissionais mostram diff, inserem no cursor ou pedem confirmação.
- **Diferença identificada:** Rótulo de inserção, efeito de substituição.
- **Por que o comportamento atual é inferior:** SQL não salvo some; o undo existe e não é anunciado nessa ação.
- **Comportamento recomendado:** Inserir no cursor/seleção, ou “Replace editor” com confirmação.
- **Evidência no projeto:** `src/app/workspace.rs` (`Insert into editor`)
- **Confiança:** Confirmado no Jykre; Provável na comparação

## N026 — Clique simples no explorer abre query; teclado não ativa o nó

- **Prioridade:** Média
- **Funcionalidade:** Explorer
- **Comportamento atual:** Clique no nome da relação chama `open_relation` (`SELECT * FROM … LIMIT 100`). Enter/Espaço/setas não fazem o equivalente. F5 é o único atalho do nó.
- **Comportamento de referência:** Gerenciadores profissionais selecionam no clique simples e abrem a tabela no duplo clique ou Enter.
- **Diferença identificada:** Ativação agressiva no mouse, nula no teclado.
- **Por que o comportamento atual é inferior:** Abas acidentais; árvore anunciada como botão que o teclado não dispara.
- **Comportamento recomendado:** Seleção no clique simples; abertura no duplo clique/Enter; setas de árvore.
- **Evidência no projeto:** `src/app/connections.rs` (`open_relation`, `on_click`, `on_key_down`); `src/backend.rs` (`preview_sql`)
- **Confiança:** Confirmado

## N027 — Enter na command palette sempre abre o primeiro resultado

- **Prioridade:** Média
- **Funcionalidade:** Busca
- **Comportamento atual:** `PressEnter` pega `.next()` da lista. Linhas só têm `on_click`. Sem setas, sem focus trap (o diálogo de conexão tem).
- **Comportamento de referência:** Palettes profissionais navegam com setas e ativam o item selecionado.
- **Diferença identificada:** Foco visual e ação de Enter divergem.
- **Por que o comportamento atual é inferior:** Com vários matches, o teclado abre o errado.
- **Comportamento recomendado:** Índice selecionado, setas, Enter no item, focus trap.
- **Evidência no projeto:** `src/app/mod.rs` (`command_subscription`)
- **Confiança:** Confirmado

## N028 — Refresh anuncia sucesso mantendo a árvore antiga

- **Prioridade:** Média
- **Funcionalidade:** Metadata
- **Comportamento atual:** Após o refresh remoto, a recarga do snapshot é tentada; erro é ignorado (`if let Ok(snapshot)`). O status usa só o resultado do refresh (“completed and saved”).
- **Comportamento de referência:** Sucesso visual implica árvore atualizada.
- **Diferença identificada:** “Publicado no cache” ≠ “carregado na UI”.
- **Por que o comportamento atual é inferior:** O usuário confia em nós velhos.
- **Comportamento recomendado:** Distinguir parcial; retry de leitura.
- **Evidência no projeto:** `src/app/connections.rs` (pós-refresh)
- **Confiança:** Confirmado

## N029 — Teste de conexão não cancela no próprio controle

- **Prioridade:** Média
- **Funcionalidade:** Diálogo de conexão
- **Comportamento atual:** O botão vira “Testing…” inerte (`connection_testing.is_some()` retorna cedo). Sem elapsed. Cancelar só editando ou fechando. Databricks: até 90 s.
- **Comportamento de referência:** Gerenciadores profissionais cancelam também a criação da conexão, não só a execução da query.
- **Diferença identificada:** Run vira Cancel; Test não.
- **Por que o comportamento atual é inferior:** Viola a regra local de cancelamento > 2 s e deixa o usuário preso.
- **Comportamento recomendado:** O mesmo botão vira Cancel; mostrar tempo após 2 s.
- **Evidência no projeto:** `src/app/connections.rs` (`test_connection`); `docs/reliability.md`
- **Confiança:** Confirmado

## N030 — Salvamento de conexão não é atômico

- **Prioridade:** Média
- **Funcionalidade:** Persistência
- **Comportamento atual:** `repository.save` incrementa revisão; depois `set_row_limit`. Se o segundo falha, a UI mostra erro com o perfil já mudado; retry usa revisão antiga e tende a conflito.
- **Comportamento de referência:** Perfil e preferências do data source gravam juntos.
- **Diferença identificada:** Dois writes, um feedback.
- **Por que o comportamento atual é inferior:** Falha enganosa e estado pela metade.
- **Comportamento recomendado:** Uma transação; só então atualizar a UI.
- **Evidência no projeto:** `src/connection_store.rs` (`save_to`)
- **Confiança:** Confirmado

## N031 — Salvar cancela o refresh antes de saber se o save funcionará

- **Prioridade:** Média
- **Funcionalidade:** Conexões × metadata
- **Comportamento atual:** Ao iniciar o save de uma conexão existente, `refreshing.take().cancel()` e `refresh_generation += 1` ocorrem **antes** do persistir.
- **Comportamento de referência:** Invalidar trabalho remoto só depois da configuração adotada.
- **Diferença identificada:** Efeito colateral antecipado.
- **Por que o comportamento atual é inferior:** Save que falha já matou uma introspecção válida.
- **Comportamento recomendado:** Cancelar refresh só após commit bem-sucedido.
- **Evidência no projeto:** `src/app/connections.rs` (início de `save_connection`)
- **Confiança:** Confirmado

## N032 — Status do gutter sobrevive a mudança de whitespace dentro de literal

- **Prioridade:** Média
- **Funcionalidade:** Gutter
- **Comportamento atual:** `normalized_statements` faz `split_whitespace` no SQL inteiro. `SELECT 'a b'` e `SELECT 'a  b'` carregam o mesmo status.
- **Comportamento de referência:** Indicador de execução invalida quando o texto executável muda.
- **Diferença identificada:** Normalização de layout vaza para literais.
- **Por que o comportamento atual é inferior:** Ícone de sucesso em SQL que nunca rodou.
- **Comportamento recomendado:** Colapsar whitespace só entre tokens; preservar literais.
- **Evidência no projeto:** `src/sql.rs` (`normalized_statements`); `src/query.rs` (`carry_statuses`)
- **Confiança:** Confirmado

## N033 — Cópias grandes bloqueiam a UI e não pedem confirmação

- **Prioridade:** Média
- **Funcionalidade:** Clipboard
- **Comportamento atual:** Seleção e CSV da página materializam no thread da UI e vão direto ao clipboard. `docs/results.md` ainda registra o desvio (50 MiB / off UI thread).
- **Comportamento de referência:** Exportação/job ou confirmação para volume grande.
- **Diferença identificada:** Cópia é ação existente sem limite operacional.
- **Por que o comportamento atual é inferior:** Pode congelar a janela.
- **Comportamento recomendado:** Estimar tamanho, confirmar acima de 50 MiB, gerar fora da UI.
- **Evidência no projeto:** `src/app/result_grid.rs` (`copy_selection`); `src/app/workspace.rs` (CSV); `docs/results.md`
- **Confiança:** Confirmado

## N034 — Busca mostra resultados da consulta anterior enquanto a nova corre

- **Prioridade:** Média
- **Funcionalidade:** Pesquisa
- **Comportamento atual:** Matches antigos permanecem de propósito (“so the list does not blink”). “Searching…” só aparece quando a lista está **vazia**. Enter ainda abre o primeiro (N027).
- **Comportamento de referência:** Busca global distingue pending do resultado atual.
- **Diferença identificada:** Falta de geração visível na lista quando já há matches.
- **Por que o comportamento atual é inferior:** A lista velha parece corresponder ao texto novo.
- **Comportamento recomendado:** Estado pending visível mesmo com resultados antigos; cursor de seleção.
- **Evidência no projeto:** `src/app/connections.rs` (`search_cache`); `src/app/mod.rs` (palette)
- **Confiança:** Confirmado

## N035 — Histórico ignora falhas

- **Prioridade:** Média
- **Funcionalidade:** Histórico
- **Comportamento atual:** A lista **persiste** (200 entradas em `workspace.state`). Só entra `WorkspaceEvent::Executed`, emitido no `complete` de statement com sucesso. Falha no primeiro statement não entra.
- **Comportamento de referência:** Gerenciadores profissionais guardam o histórico dos statements rodados no console, inclusive tentativas que falharam.
- **Diferença identificada:** Persistência existe; o recorte continua sendo só o que deu certo.
- **Por que o comportamento atual é inferior:** Some justamente o SQL que se precisa reabrir para corrigir — agora de forma durável: a falha nunca chega ao arquivo.
- **Comportamento recomendado:** Registrar tentativa com status, horário, conexão e duração.
- **Evidência no projeto:** `src/app/workspace.rs` (emit `Executed` só após `complete`); `src/app/mod.rs` (`history`); `src/workspace_state.rs`
- **Confiança:** Confirmado
- **Nota:** Parcial — persistência e limite 200 implementados; falhas continuam de fora.

## N036 — Execução longa não mostra tempo decorrido

- **Prioridade:** Baixa
- **Funcionalidade:** Progresso
- **Comportamento atual:** Footer: “Running…”. `RunState::Running { started_at }` já existe. Timeout agora é escolhido por aba (30 s a 1 h, ou nenhum), o que torna o relógio mais necessário. Desvio de `docs/ux.md`.
- **Comportamento de referência:** Gerenciadores profissionais mostram a duração do job ativo e, em geral, notificam execuções longas.
- **Diferença identificada:** O relógio está no estado e não na UI.
- **Por que o comportamento atual é inferior:** Sem noção de proximidade do timeout.
- **Comportamento recomendado:** Mostrar elapsed após 2 s, derivado de `started_at`.
- **Evidência no projeto:** `src/query.rs`; `src/app/workspace.rs` (footer)
- **Confiança:** Confirmado

## N037 — Grupos vazios aparecem ou desaparecem conforme o histórico de abas

- **Prioridade:** Baixa
- **Funcionalidade:** Tab groups
- **Comportamento atual:** Toda conexão ganha grupo. Fechar a última aba remove o grupo (`model.close`). O render ainda trata `group.tabs.is_empty()` (“No tabs”). Reabrir recria no fim da barra.
- **Comportamento de referência:** O grupo acompanha o data source ou some de forma estável.
- **Diferença identificada:** Duas conexões sem abas podem ter representações diferentes.
- **Por que o comportamento atual é inferior:** A ordem da barra muda ao reabrir a última query.
- **Comportamento recomendado:** Grupo com a vida da conexão, ou ocultar vazios sempre.
- **Evidência no projeto:** `src/grouped_tabs/model.rs` (`close`); `src/grouped_tabs/mod.rs`; `src/app/mod.rs` (`add_query` / `ensure_group`)
- **Confiança:** Confirmado

## N038 — Explorer materializa todos os nós visíveis a cada render

- **Prioridade:** Baixa
- **Funcionalidade:** Explorer / responsividade
- **Comportamento atual:** Filtra a coleção e cria um elemento por objeto visível (`.children(objects…)`). Páginas de 200, expansões sucessivas. Desvio em `docs/ui-guidelines.md`.
- **Comportamento de referência:** Árvores grandes são virtualizadas/lazy.
- **Diferença identificada:** A grade é virtualizada; o explorer não.
- **Por que o comportamento atual é inferior:** Foco, busca e refresh degradam em catálogos grandes.
- **Comportamento recomendado:** Lista achatada virtualizada por `ObjectPath`.
- **Evidência no projeto:** `src/app/connections.rs` (render da árvore); `docs/ui-guidelines.md`
- **Confiança:** Confirmado (risco estrutural; sem medição deste cenário)

## N039 — Tamanho da fonte pode gravar fora de ordem e esconder a falha

- **Prioridade:** Baixa
- **Funcionalidade:** Preferências
- **Comportamento atual:** Cada Ctrl+/− dispara write sem geração. Writes rápidos podem completar invertidos. Erro vai para `app_notice`, renderizado no Explorer, não em Preferences.
- **Comportamento de referência:** Preferência com debounce e erro junto do controle.
- **Diferença identificada:** A UI pode mostrar 18 px e reiniciar em 14.
- **Por que o comportamento atual é inferior:** Feedback longe da ação, persistência não-monotônica.
- **Comportamento recomendado:** Última geração vence; aviso global ou inline em Preferences.
- **Evidência no projeto:** `src/app/connections.rs` (`persist_font_size`); `src/app/mod.rs` (`capture_key_down`)
- **Confiança:** Confirmado

## N040 — A seta de ordenação do demo só inverte a ordem recebida

- **Prioridade:** Baixa
- **Funcionalidade:** Grade (demo)
- **Comportamento atual:** Clique na terceira coluna do resultado demo alterna `descending`. `GridView` faz `rows.reverse()`, sem comparar a coluna. Em resultado real o clique no header não ordena.
- **Comportamento de referência:** Seta no header = sort pelos valores da coluna.
- **Diferença identificada:** Affordance de sort por coluna, operação de reverse.
- **Por que o comportamento atual é inferior:** Primeiro contato (demo) ensina o gesto errado.
- **Comportamento recomendado:** Remover a seta do demo ou ordenar de verdade, com regra estável para NULL.
- **Evidência no projeto:** `src/app/result_grid.rs` (header `index == 2 && sales`); `src/grid.rs`
- **Confiança:** Confirmado

## N041 — Chamada ao vault sem timeout pode prender o diálogo de salvamento

- **Prioridade:** Baixa
- **Funcionalidade:** Vault / diálogo
- **Comportamento atual:** Durante `connection_saving` o diálogo não fecha. `secrets.set` é `spawn_blocking` sem timeout.
- **Comportamento de referência:** Operação de credencial tem limite e permite dispensar o modal.
- **Diferença identificada:** Save vira espera ilimitada no keyring do SO.
- **Por que o comportamento atual é inferior:** Backend de credenciais travado deixa o modal sem saída.
- **Comportamento recomendado:** Timeout operacional; fechar e avisar depois.
- **Evidência no projeto:** `src/app/connections.rs` (`close_connection_dialog`); `crates/secrets/src/lib.rs`
- **Confiança:** Provável (depende do backend do SO)

---

## Sequência recomendada do que resta

1. **Diagnóstico e SQL enviado — N002, N010, N011, N015.** Sem mensagem real e com splitter/format errados, o novo modelo de cursor/sessão ainda pode mandar o SQL errado ou esconder o erro.
2. **Autenticação e carga — N012, N013, N014, N029, N030.** Credencial no destino errado e explorer vazio.
3. **Fechar e biblioteca — N006 (SQL/teclado), N018, N035, N005.** O que a persistência nova ainda deixa cair no chão.
4. **Teclado e explorer — N019, N026, N027.**
5. **Grade e gutter — N021, N022, N023, N032.**
6. **Feedback restante — N024, N025, N028, N031, N033, N034, N036–N041.**

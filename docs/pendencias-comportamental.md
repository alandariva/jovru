# Auditoria comportamental — Jykre Studio × ferramentas de referência

Data: 3 de novembro de 2026 · Branch `release/0.1.0` (`15446f2` + alterações locais não commitadas). Este arquivo substitui o backlog anterior da auditoria comportamental, que continua no histórico do Git.

**Ferramentas de referência.** A comparação usa três gerenciadores de banco de dados amplamente adotados no mercado, chamados aqui de **Ferramenta A** (IDE comercial de banco de dados), **Ferramenta B** (cliente universal open source) e **Ferramenta C** (cliente nativo leve). Os comportamentos atribuídos a cada uma foram conferidos na documentação oficial delas.

**Como foi feita.** Leitura do código das views (`src/app/*`), do modelo (`query.rs`, `result_tabs.rs`, `sql.rs`, `grid.rs`, `workspace_state.rs`, `connection_store.rs`, `grouped_tabs/`) e dos documentos em `docs/`. **A aplicação não foi executada**: tudo que está como *Confirmado* foi confirmado no código.

**Como ler a referência.** Cada item diz quais ferramentas documentam o comportamento de referência. “Não documentado” significa que a documentação consultada não cobre o caso; não significa que a ferramenta se comporte diferente. Quando as ferramentas divergem entre si, o item diz isso, e o achado só se sustenta se o Jykre for pior que todas as abordagens ou inconsistente consigo mesmo. Alguns itens não dependem de nenhuma ferramenta: são violações de princípios gerais (não executar algo diferente do pedido, não perder dados em silêncio, consistência interna). Nesses casos a referência aparece como **Princípio**.

**Aprovação.** Cada pendência tem a propriedade **Aprovação**. Só as pendências **Aprovadas** estão ativas para desenvolvimento; as **Pendentes** aguardam a decisão do mantenedor e não devem ser implementadas. Quando a aprovação vem com uma decisão que muda a recomendação, o item tem uma linha **Decisão**, e ela prevalece sobre o *Comportamento recomendado*. Uma pendência implementada tem a linha **Status**, com a data, os commits e o desvio em relação à decisão, quando houver.

**Relação com o backlog anterior.** A versão anterior deste arquivo (até `15446f2`) usava números `Nxxx` próprios. Esta auditoria renumera tudo a partir de N001. Itens daquele backlog que **continuam abertos no código atual** aparecem de novo, marcados com “Backlog: Nxxx” (o número antigo). Dois itens que o backlog ainda listava como abertos já estão corrigidos no código: N028 (o status do refresh agora avisa quando a árvore não recarregou, `node_status`) e N031 (salvar só cancela o refresh depois de gravar). Ambos ficaram fora desta lista.

---

## 1. Resumo executivo

**Aderência: média.** O núcleo de execução já segue o que as três ferramentas fazem: executar a instrução sob o cursor ou a seleção, um resultado por instrução, erro do servidor com detalhes. Em alguns pontos o Jykre é mais cuidadoso que as referências (seção 4).

As divergências que pesam ficam nas bordas desse núcleo:

1. **Gatilhos que alternam sentido.** O atalho de executar também cancela, e o botão de salvar consulta também apaga. Com isso, um segundo toque desfaz o primeiro.
2. **Resultado desligado de onde veio.** “Run query again” roda o que está no cursor, não o SQL do resultado. `Ctrl+Enter` sobrescreve o último tab criado, inclusive o que o usuário quis manter; A e B têm um “fixar resultado” que impede isso.
3. **Identidade por nome, não por ID.** Abas, histórico e consultas salvas apontam para a conexão pelo nome. Se o nome não existe mais, o histórico abre na *primeira* conexão, e a aba órfã roda o *demo*. A e C mantêm o histórico por conexão.
4. **Fluxos de conexão sem diálogo.** Senha não lembrada nunca é pedida (A e B pedem ao conectar). Salvar o perfil derruba os runs abertos sem avisar.
5. **Abrir tabela não mostra dados.** Nas três ferramentas, abrir uma tabela leva aos dados (ou ao editor do objeto) com paginação. No Jykre, cria uma aba com `SELECT … LIMIT 100` que não é executada.

São 47 divergências relevantes: 10 de prioridade alta, 22 média e 15 baixa, sem nenhuma crítica. Dessas, 15 são reconfirmações do backlog anterior. **46 estão aprovadas**. N001–N016 foram aprovadas em 3 de novembro de 2026 e estão **implementadas** (`f18136c`, de 3 de novembro, com `8208602` para N008 e `66f262b` para N010, de 4 de novembro). N017–N047, exceto N043, foram aprovadas em 4 de novembro de 2026: 29 estão **implementadas** (alterações locais não commitadas de 4 de novembro; N040 e N044 já eram atendidas pelo código atual e foram só conferidas) e N031 está **parcial** (a confirmação acima de 50 MiB foi feita; gerar a cópia fora da thread da UI depende de uma decisão sobre o ADR-0012). N043 foi **reprovada**.

---

## 2. Divergências

### N001 — O atalho de executar cancela a execução em andamento, num toque e sem confirmação
- **Prioridade:** Alta
- **Aprovação:** Aprovada (3 de novembro de 2026)
- **Status:** Implementada (3 de novembro de 2026; `f18136c`).
- **Funcionalidade:** Execução / cancelamento
- **Comportamento atual:** Com um run ativo, `Ctrl+Enter`, `Ctrl+Shift+Enter` e `Ctrl+Alt+Enter` chamam `stop_execution`. O mesmo vale para o botão do gutter de uma instrução *Pending* e para um segundo clique em Run (que vira “Stop” no mesmo lugar). O cancelamento é imediato e sem confirmação.
- **Comportamento de referência:** As ferramentas divergem quanto ao botão, mas nenhuma documenta cancelar pelo atalho de executar:
  - **A:** cancelar é uma ação separada; um segundo pedido de cancelamento abre um aviso antes de encerrar o processo no servidor.
  - **C:** o botão Run vira Cancel durante a execução, mas Cancel abre a lista de processos para escolher qual encerrar.
  - **B:** não documentado.
- **Diferença identificada:** O botão que alterna é aceitável (C faz igual). O problema é o *atalho* de executar passar a cancelar, num toque só.
- **Por que o comportamento atual é inferior:** Repetir `Ctrl+Enter` (para “rodar de novo” ou porque o primeiro toque parece não ter respondido) cancela um `UPDATE`/`DELETE` longo no meio. O usuário quis executar e o efeito foi interromper.
- **Comportamento recomendado:** Atalhos de execução nunca cancelam. Com run ativo, avisar “já em execução” (como o projeto já faz durante o Cancelling). Cancelar fica no botão e num atalho próprio.
- **Decisão:** Acionar de novo um atalho de execução (`Ctrl+Enter`, `Ctrl+Shift+Enter`, `Ctrl+Alt+Enter`) durante um run abre uma confirmação “cancelar a execução?” com o foco já em **Sim**, para que Enter confirme. A confirmação de `Ctrl+Alt+Enter`, que executa todas as instruções, para a execução inteira.
- **Evidência no projeto:** `src/app/workspace.rs` — `toggle_run`, `execute_current`, botão `execute` em `toolbar`, `run_button` (Pending → `stop_execution`).
- **Confiança:** Confirmado no projeto; referência documentada em A e C.

### N002 — “Run query again” executa a instrução do cursor, não a do resultado
- **Prioridade:** Alta
- **Aprovação:** Aprovada (3 de novembro de 2026)
- **Status:** Implementada (3 de novembro de 2026; `f18136c`).
- **Funcionalidade:** Resultados
- **Comportamento atual:** O botão ↻ “Run query again”, na barra do result tab, chama `execute_current`. Isso roda a seleção ou a instrução sob o cursor do editor *agora* e grava o resultado no último tab criado. O SQL guardado no próprio tab (`tab.sql`) não é usado.
- **Comportamento de referência:** Princípio: reexecutar a partir de um resultado repete o que gerou aquele resultado. Nenhuma das três documenta um botão equivalente; a referência é a expectativa criada pelo próprio rótulo.
- **Diferença identificada:** O rótulo promete repetir o resultado visível; a ação usa o estado do editor.
- **Por que o comportamento atual é inferior:** Com o cursor em outra instrução, o “refresh” de um `SELECT` pode rodar um `DELETE`. E o resultado vai para outro tab, não para o que está na tela.
- **Comportamento recomendado:** Reexecutar `tab.sql` do tab ativo e colocar o resultado nesse mesmo tab.
- **Evidência no projeto:** `src/app/workspace.rs` — botão `refresh-results` → `execute_current`; `ResultTab::sql` (`src/result_tabs.rs`).
- **Confiança:** Confirmado.

### N003 — Abrir uma tabela não mostra os dados
- **Prioridade:** Alta
- **Aprovação:** Aprovada (3 de novembro de 2026)
- **Status:** Implementada (3 de novembro de 2026; `f18136c`). Atualização (5 de novembro de 2026): substituída a pedido do mantenedor pela vista **Dados** (“Visualizador de dados da tabela”): duplo clique, Enter, a palette e **View data** abrem uma aba com `SELECT * FROM …;`, sem `LIMIT`, que executa a leitura na hora num result tab chamado Data e pagina pelo cursor da sessão, com o SQL visível no editor. **New query for table** continua no menu, abrindo outra aba com o SQL e o `LIMIT`, sem executar.
- **Funcionalidade:** Explorer / navegação
- **Comportamento atual:** Duplo clique, Enter, “Open preview” ou Enter na palette chamam `add_query` com `SELECT * FROM … LIMIT 100;`. Abre uma aba de consulta com o SQL e o estado vazio “Ready to explore — Run the query”. Nada é executado.
- **Comportamento de referência:** Nas três ferramentas, abrir a tabela leva aos dados ou ao editor do objeto:
  - **A:** duplo clique ou F4 abrem o editor de dados da tabela.
  - **B:** abre o editor do objeto; a página inicial (Dados, Propriedades…) é configurável.
  - **C:** clicar no nome da tabela abre os dados, paginados (300 linhas por página por padrão).
- **Diferença identificada:** O gesto de “abrir” entrega um rascunho de SQL, não os dados.
- **Por que o comportamento atual é inferior:** Toda inspeção de tabela custa um passo a mais (`Ctrl+Enter`). O nome “Open preview” promete uma prévia que não aparece.
- **Comportamento recomendado:** Executar a prévia ao abrir, ou renomear a ação para “New query for table”.
- **Decisão:** A ação passa a se chamar **New query for table** e continua só criando a aba com o SQL, sem executar. O rótulo muda em todos os pontos de entrada (menu de contexto, duplo clique ou Enter no explorer, palette).
- **Evidência no projeto:** `src/app/connections.rs` — `open_relation`; `src/app/mod.rs` — `add_query`, palette (`PressEnter`); `src/backend.rs` — `preview_sql`.
- **Confiança:** Confirmado no projeto; referência documentada nas três ferramentas.

### N004 — Senha não lembrada nunca é solicitada ao conectar
- **Prioridade:** Alta
- **Aprovação:** Aprovada (3 de novembro de 2026)
- **Status:** Implementada (3 de novembro de 2026; `f18136c`).
- **Funcionalidade:** Autenticação
- **Comportamento atual:** Sem “Remember password” e após reiniciar, a conexão carrega sem credencial. O primeiro run (ou refresh) falha com “Could not sign in”. O único caminho é abrir *Configure connection*, digitar a senha e salvar o perfil; o aviso do save chega a dizer “Enter the password before querying the server”.
- **Comportamento de referência:**
  - **A:** quando a senha não é guardada, ela é pedida a cada conexão (ou guardada até reiniciar, conforme a opção escolhida).
  - **B:** quem não marca “salvar credenciais” recebe o pedido de credenciais ao conectar.
  - **C:** não documentado (as credenciais ficam no keychain do sistema).
- **Diferença identificada:** Não guardar a senha vira um erro, não um prompt.
- **Por que o comportamento atual é inferior:** Quem segue a boa prática de não salvar a senha passa, toda sessão, por uma falha seguida de edição do perfil, e salvar o perfil dispara efeitos colaterais (N005).
- **Comportamento recomendado:** Ao abrir uma sessão sem credencial, pedir a senha num diálogo próprio. A senha fica em memória até reiniciar, com a opção de lembrar.
- **Evidência no projeto:** `src/connection_store.rs` — `save_to`; `src/app/connections.rs` — `form_input`; `src/error_report.rs` (ação de `Authentication`); não existe nenhum prompt de senha em `src/app`.
- **Confiança:** Confirmado no projeto; referência documentada em A e B.

### N005 — Salvar a conexão cancela runs e apaga resultados de todas as abas, sem aviso
- **Prioridade:** Alta
- **Aprovação:** Aprovada (3 de novembro de 2026)
- **Status:** Implementada (3 de novembro de 2026; `f18136c`).
- **Funcionalidade:** Conexões × abas
- **Comportamento atual:** Quando qualquer opção do perfil muda (host, porta, banco padrão, sslMode…), `update_connection` chama `set_connection` em cada aba daquela conexão. O resultado: `abandon_run` (cancela o run, fecha a sessão e reverte uma transação aberta), `results.clear()` e o assistente fechado. Nenhuma confirmação antes e nenhuma notificação na aba depois.
- **Comportamento de referência:** Princípio: uma edição de configuração não destrói trabalho em andamento sem perguntar. O próprio Jykre segue esse princípio ao mover uma aba de conexão (pergunta antes). As três ferramentas não documentam o efeito de editar um data source conectado.
- **Diferença identificada:** Uma edição de configuração tem efeito destrutivo e imediato em trabalho em andamento.
- **Por que o comportamento atual é inferior:** Corrigir um detalhe do perfil durante um `UPDATE` longo o cancela, e todos os resultados abertos somem.
- **Comportamento recomendado:** Se houver run ou transação aberta nas abas afetadas, perguntar antes de salvar. Fora isso, manter resultados e sessão até o próximo run e avisar na aba que a configuração mudou.
- **Evidência no projeto:** `src/app/connections.rs` — `save_connection` (laço `panel.update_connection`); `src/app/workspace.rs` — `update_connection`, `set_connection`, `abandon_run`.
- **Confiança:** Confirmado no projeto; referência por princípio e por consistência interna.

### N006 — Consulta salva ou do histórico abre em outra conexão quando a original não existe
- **Prioridade:** Alta
- **Aprovação:** Aprovada (3 de novembro de 2026)
- **Status:** Implementada (3 de novembro de 2026; `f18136c`).
- **Funcionalidade:** Biblioteca / histórico
- **Comportamento atual:** O clique numa entrada procura a conexão pelo nome e, se não encontra, usa `.or(this.connections.first())`: abre a aba na primeira conexão da lista, sem aviso.
- **Comportamento de referência:**
  - **A:** o histórico pertence ao console da conexão.
  - **C:** o histórico é mantido separadamente por conexão.
  - **B:** não documentado.
- **Diferença identificada:** Falta da conexão de origem vira troca de conexão.
- **Por que o comportamento atual é inferior:** SQL escrito para um banco (às vezes um `DELETE`) fica pronto para rodar em outro. Isso acontece quando um perfil não carrega na inicialização ou foi renomeado fora da aplicação.
- **Comportamento recomendado:** Abrir sem conexão (como já é feito para abas órfãs) ou pedir que o usuário escolha a conexão.
- **Evidência no projeto:** `src/app/mod.rs` — `sidebar_content`, `on_click` das entradas (`.or(this.connections.first())`).
- **Confiança:** Confirmado no projeto; referência documentada em A e C.

### N007 — Uma aba sem conexão executa o SQL contra dados simulados
- **Prioridade:** Alta
- **Aprovação:** Aprovada (3 de novembro de 2026)
- **Status:** Implementada (3 de novembro de 2026; `f18136c`).
- **Funcionalidade:** Execução / estado de conexão
- **Comportamento atual:** Com `input == None`, `execute_queries` cai no ramo do demo: `sample_result` gera linhas fictícias, `preview_error` recusa o que não é leitura e o rodapé mostra “Demo data · MOCK”. Fora do modo `JYKRE_DEMO`, isso acontece em abas restauradas cuja conexão não existe mais (grupo órfão) ou movidas para “Without connection”. O cabeçalho mostra o banco inventado pela heurística do demo (`commerce`/`finance`/`analytics`).
- **Comportamento de referência:** Princípio: sem conexão não há execução; a ferramenta pede uma conexão.
- **Diferença identificada:** Código de demonstração atende um caminho real.
- **Por que o comportamento atual é inferior:** O usuário vê linhas plausíveis que não vieram de banco nenhum, ou a mensagem “The demo accepts only read queries”, que não faz sentido fora do demo.
- **Comportamento recomendado:** Fora do demo, Run numa aba sem conexão mostra “Choose a connection for this tab” e abre o seletor de conexão.
- **Evidência no projeto:** `src/app/workspace.rs` — `execute_queries` (ramo após `if let Some(input)`), `Workspace::new` (`database`/`connection_color` por nome); `src/app/state.rs` — `group_for`; `src/app/demo/mod.rs` — `sample_result`.
- **Confiança:** Confirmado.

### N008 — Transação explícita fica invisível e é revertida ao fechar a aba (Backlog: N005)
- **Prioridade:** Alta
- **Aprovação:** Aprovada (3 de novembro de 2026)
- **Status:** Implementada (3 e 4 de novembro de 2026; `f18136c`, `8208602`). Desvio: o estado vem do driver (`Session::transaction`): o PostgreSQL consulta o servidor sem tirar o snapshot, e o MySQL informa só `autocommit = 0`. O que o driver não sabe (um `BEGIN` no MySQL) é inferido pelas instruções. A leitura do protocolo está registrada como débito crítico [TD-001](technical-debt.md#td-001--transaction-state-is-not-read-from-the-protocol).
- **Funcionalidade:** Transações
- **Comportamento atual:** Só autocommit é suportado. Um `BEGIN` continua aberto na sessão da aba, sem indicador e sem Commit/Rollback. Fechar a aba, trocar de conexão, salvar o perfil (N005) ou perder a sessão reverte a transação em silêncio. A mensagem de falha parcial afirma “Previous statements were not rolled back automatically”, o que é falso dentro de uma transação abortada no PostgreSQL.
- **Comportamento de referência:**
  - **B:** alterna entre auto-commit e commit manual, mostra se há alterações pendentes e tem botões Commit e Rollback, além da lista de transações pendentes.
  - **A:** expõe o modo de transação e os comandos de Commit e Rollback (Provável; não conferido nesta auditoria).
  - **C:** não documentado.
- **Diferença identificada:** O estado transacional existe na sessão, mas não aparece na interface.
- **Por que o comportamento atual é inferior:** Trabalho pode ser descartado sem o usuário saber, ou dado como persistido quando não foi.
- **Comportamento recomendado:** Detectar uma transação aberta e mostrá-la. Confirmar antes de fechar, mover ou reconfigurar. Não afirmar nada sobre rollback que não tenha sido observado.
- **Decisão:** Aprovada como recomendado. Além disso, a interface mostra o estado da transação com ícones (por exemplo: auto-commit, transação aberta, alterações pendentes), como fazem as ferramentas de referência.
- **Evidência no projeto:** `src/query.rs` (mensagem “not rolled back”); `src/app/workspace.rs` — `abandon_run`, `Drop`; `docs/architecture.md` (Known deviations).
- **Confiança:** Confirmado no projeto; referência documentada em B.

### N009 — A separação de instruções ignora o dialeto (Backlog: N010)
- **Prioridade:** Alta
- **Aprovação:** Aprovada (3 de novembro de 2026)
- **Status:** Implementada (3 de novembro de 2026; `f18136c`). Desvio: o splitter segue o dialeto do driver (`SqlDialect`), mas fica na aplicação (`jykre::sql`), não no driver; blocos são reconhecidos por palavra-chave. Ver os Known deviations de [Architecture](architecture.md#known-deviations).
- **Funcionalidade:** SQL enviado
- **Comportamento atual:** Todo `;` fora de aspas, comentário ou dollar-quote encerra uma instrução. Corpos `BEGIN … END` de procedure/trigger do MySQL e `BEGIN ATOMIC` do PostgreSQL são fatiados. Não há `DELIMITER`, e um `/*! … */` inicial é descartado como comentário.
- **Comportamento de referência:** As ferramentas divergem:
  - **B:** delimitador configurável e linha em branco como delimitador (sempre, nunca ou “inteligente”).
  - **A:** separa por dialeto (Provável).
  - **C:** documenta apenas a separação por ponto e vírgula, como o Jykre.
- **Diferença identificada:** O Jykre está no nível de C e abaixo de A e B, num produto que já tem três dialetos.
- **Por que o comportamento atual é inferior:** Rotinas nunca chegam inteiras ao servidor, e `Ctrl+Enter` escolhe um fragmento como “a instrução”.
- **Comportamento recomendado:** Splitter fornecido pelo driver (dialeto), com rotinas, `DELIMITER` e comentários executáveis.
- **Evidência no projeto:** `src/sql.rs` — `statements`; `docs/architecture.md` (Known deviations).
- **Confiança:** Confirmado no projeto; referência documentada em B.

### N010 — Format pode alterar strings do MySQL e joga o cursor para o início (Backlog: N011)
- **Prioridade:** Alta
- **Aprovação:** Aprovada (3 de novembro de 2026)
- **Status:** Implementada (3 e 4 de novembro de 2026; `f18136c`, `66f262b`). A formatação usa `sqruff-lib` 0.44, escolhida pelo mantenedor, só com regras de layout e de caixa de palavras-chave, fora da thread da UI.
- **Funcionalidade:** Formatação
- **Comportamento atual:** `tokens()` (usado por `format`) não trata `\` dentro de strings; `statements()` trata. Isso faz com que `'a\'b'` seja tokenizado de forma diferente nos dois caminhos. A aplicação usa `replace_all`, que reposiciona o caret e zera o scroll.
- **Comportamento de referência:** Princípio: formatar nunca muda o significado de um literal e não tira o usuário do ponto de edição.
- **Diferença identificada:** Dois tokenizers que discordam, e uma reescrita do buffer inteiro.
- **Por que o comportamento atual é inferior:** Formatar pode mudar o significado de um literal e sempre tira o usuário do ponto em que estava.
- **Comportamento recomendado:** Um tokenizer único por dialeto; restaurar caret, seleção e scroll depois de formatar.
- **Decisão:** Aprovada como recomendado. Avaliar uma dependência dedicada à formatação de SQL em vez do formatador próprio, seguindo a política de [Stack and dependencies](stack-and-dependencies.md); a escolha da dependência é apresentada ao mantenedor antes de entrar.
- **Evidência no projeto:** `src/sql.rs` — `tokens` vs `statements`; `src/app/workspace.rs` — botão `format`.
- **Confiança:** Confirmado.

### N011 — A prévia de tabela tem `LIMIT 100` fixo e ignora o limite de linhas da conexão
- **Prioridade:** Média
- **Aprovação:** Aprovada (3 de novembro de 2026)
- **Status:** Implementada (3 de novembro de 2026; `f18136c`).
- **Funcionalidade:** Explorer / paginação
- **Comportamento atual:** `preview_sql` sempre gera `SELECT * FROM x LIMIT 100;`. O limite por conexão (1 a 100.000, padrão 1.000) e a paginação ficam sem efeito: a página 1 tem 100 linhas e não existe próxima.
- **Comportamento de referência:**
  - **A:** o editor de dados pagina a tabela com tamanho de página configurável e navegação entre páginas.
  - **C:** pagina a tabela, 300 linhas por página por padrão, com limit/offset ajustáveis.
  - **B:** não conferido.
- **Diferença identificada:** Um limite embutido no SQL anula o mecanismo de páginas do próprio produto.
- **Por que o comportamento atual é inferior:** Para ver a linha 101, o usuário precisa editar o SQL; a configuração “Row limit” parece não funcionar.
- **Comportamento recomendado:** Gerar `SELECT * FROM x` e deixar o limite de linhas e a paginação controlarem o volume.
- **Decisão:** A consulta gerada mantém um `LIMIT` por padrão, com o valor do limite de linhas da conexão em vez do 100 fixo. Atualização (5 de novembro de 2026): vale só para **New query for table**; a vista Dados não escreve `LIMIT` no texto e lê as páginas seguintes pela sessão (ver N003).
- **Evidência no projeto:** `src/backend.rs` — `preview_sql`; `docs/results.md` (Limits).
- **Confiança:** Confirmado no projeto; referência documentada em A e C.

### N012 — `Ctrl+Enter` sobrescreve justamente o result tab aberto com `Ctrl+Shift+Enter`
- **Prioridade:** Média
- **Aprovação:** Aprovada (3 de novembro de 2026)
- **Status:** Implementada (3 de novembro de 2026; `f18136c`).
- **Funcionalidade:** Result tabs
- **Comportamento atual:** `ReplaceLast` reaproveita o tab de maior número (`last_created`), seja ele o visível ou não. Um tab aberto com `Ctrl+Shift+Enter` para preservar um resultado é o primeiro a ser substituído no próximo `Ctrl+Enter`. Com Result 1 na tela, o novo resultado vai para o Result 3.
- **Comportamento de referência:**
  - **A:** reaproveita o tab de resultado por padrão e permite fixar um tab para mantê-lo.
  - **B:** abre o resultado em tab novo por atalho próprio; um tab fixado não pode ser sobrescrito por uma nova execução.
  - **C:** não documentado.
- **Diferença identificada:** O mecanismo de “manter” (abrir um tab novo) não protege o tab, e o alvo da substituição não é o que o usuário está vendo.
- **Por que o comportamento atual é inferior:** O resultado que o usuário quis preservar some, e o que estava na tela continua desatualizado.
- **Comportamento recomendado:** Substituir o tab ativo (ou o último não fixado) e oferecer um “fixar” explícito que nenhum `Ctrl+Enter` sobrescreva.
- **Evidência no projeto:** `src/result_tabs.rs` — `begin`, `last_created` e o teste com `["c"], Placement::New` seguido de `["d","e"], ReplaceLast`.
- **Confiança:** Confirmado no projeto; referência documentada em A e B.

### N013 — Com o cursor fora de qualquer instrução, a instrução vizinha é executada
- **Prioridade:** Média
- **Aprovação:** Aprovada (3 de novembro de 2026)
- **Status:** Implementada (3 de novembro de 2026; `f18136c`).
- **Funcionalidade:** Editor / execução
- **Comportamento atual:** `statement_at` escolhe a próxima instrução quando o cursor está numa linha entre duas, e a mais próxima antes da primeira ou depois da última. Executa sem perguntar.
- **Comportamento de referência:**
  - **A:** opção explícita para o cursor fora de instrução: não executar, executar o script inteiro ou executar tudo abaixo do cursor. Nenhuma delas é “a vizinha mais próxima”.
  - **B:** linhas em branco podem delimitar instruções, com regra configurável.
  - **C:** não documentado.
- **Diferença identificada:** Uma heurística implícita no lugar de uma regra previsível e configurável.
- **Por que o comportamento atual é inferior:** Um `Ctrl+Enter` numa linha em branco executa a instrução de baixo, que pode ser destrutiva.
- **Comportamento recomendado:** Fora de instrução, não executar e avisar (ou destacar a candidata e pedir confirmação). A escolha pode ficar configurável.
- **Decisão:** Com o cursor fora de qualquer instrução, não executar e avisar o usuário. Sem confirmação nem configuração.
- **Evidência no projeto:** `src/sql.rs` — `statement_at`; `src/query.rs` — `Scope::Cursor`.
- **Confiança:** Confirmado no projeto; referência documentada em A e B (o padrão de A não é documentado).

### N014 — O filtro da grade busca só na página carregada, sem dizer isso
- **Prioridade:** Média
- **Aprovação:** Aprovada (3 de novembro de 2026)
- **Status:** Implementada (3 de novembro de 2026; `f18136c`).
- **Funcionalidade:** Grade
- **Comportamento atual:** “Filter results…” filtra no cliente apenas as linhas da página atual (até o limite de linhas). Quando a linha procurada está em outra página, a grade diz “No results for this selection. The query or the current filter returned no rows.”
- **Comportamento de referência:**
  - **B:** filtros escritos como critérios SQL (o `WHERE` fica implícito), ou seja, aplicados à consulta.
  - **A:** filtro aplicado à consulta (Provável).
  - **C:** não conferido.
- **Diferença identificada:** A busca parece global, mas é local à página.
- **Por que o comportamento atual é inferior:** O “nada encontrado” é falso quando há mais páginas ou o limite foi atingido.
- **Comportamento recomendado:** Indicar o escopo (“nesta página: 1.000 linhas”) e, havendo mais páginas, avisar que o filtro não as cobre.
- **Evidência no projeto:** `src/app/result_grid.rs` — `set_filter`; `src/grid.rs` — `GridView::new`.
- **Confiança:** Confirmado no projeto; referência Provável.

### N015 — Trocar de result tab perde a seleção e herda o scroll do outro tab
- **Prioridade:** Média
- **Aprovação:** Aprovada (3 de novembro de 2026)
- **Status:** Implementada (3 de novembro de 2026; `f18136c`).
- **Funcionalidade:** Grade / result tabs
- **Comportamento atual:** Um único `ResultGrid` atende todos os result tabs. `set_page` limpa a seleção a cada troca. O `UniformListScrollHandle` é o mesmo para todos, então o deslocamento do tab anterior passa para o próximo.
- **Comportamento de referência:** Em B, cada result tab é uma instância própria do editor de dados. Nas outras duas ferramentas, não documentado.
- **Diferença identificada:** O estado da grade pertence ao componente, não ao tab.
- **Por que o comportamento atual é inferior:** Comparar Result 1 e Result 2 alternando entre eles destrói a seleção, e a posição de leitura salta.
- **Comportamento recomendado:** Guardar seleção e scroll por `ResultId` e restaurar ao voltar.
- **Evidência no projeto:** `src/app/result_grid.rs` — `set_page`, campo `scroll`; `src/app/workspace.rs` — `show_active`.
- **Confiança:** Confirmado para a seleção; Provável para o scroll.

### N016 — Messages não é um log: o desfecho de um DML some quando o tab é substituído
- **Prioridade:** Média
- **Aprovação:** Aprovada (3 de novembro de 2026)
- **Status:** Implementada (3 de novembro de 2026; `f18136c`).
- **Funcionalidade:** Saída / mensagens
- **Comportamento atual:** Messages lista apenas os result tabs existentes (status e SQL), sem horário nem duração. Quando `Ctrl+Enter` substitui um tab, ou `Ctrl+Alt+Enter` substitui todos, o “5 rows affected” do `UPDATE` anterior desaparece. O histórico também não guarda linhas afetadas.
- **Comportamento de referência:**
  - **B:** registra cada execução com horário, duração, linhas afetadas e erros, independentemente dos tabs.
  - **A:** mantém uma saída de execução no console (Provável).
  - **C:** não documentado.
- **Diferença identificada:** A informação de execução está presa ao ciclo de vida do tab.
- **Por que o comportamento atual é inferior:** Some a prova do que foi alterado, justamente nas instruções que não retornam linhas.
- **Comportamento recomendado:** Messages como log da sessão da aba (horário, instrução resumida, linhas, duração, erro), independente dos tabs.
- **Evidência no projeto:** `src/app/workspace.rs` — `result_messages`; `src/result_tabs.rs` — `begin`.
- **Confiança:** Confirmado no projeto; referência documentada em B.

### N017 — O caminho acima do editor não mostra o contexto real da sessão
- **Prioridade:** Média
- **Aprovação:** Aprovada (4 de novembro de 2026)
- **Status:** Implementada (4 de novembro de 2026; alterações locais não commitadas). O seletor de contexto do cabeçalho, que mostra o schema/banco/catálogo informado pela sessão após cada run, já vinha de `ed9173a`; esta alteração retira os segmentos que não eram reais: sem conexão, o cabeçalho mostra só o nome da aba de conexão (sem “— / SQL”), e o rodapé do editor mostra “SQL” sem dialeto quando ele não é conhecido. O demo mantém o banco de exemplo.
- **Funcionalidade:** Contexto de conexão
- **Comportamento atual:** O cabeçalho do editor mostra “conexão / banco / SQL”. O banco vem do perfil (`options.database`/`defaultCatalog`, ou “—”). Até haver conexão, vem da heurística do demo pelo nome (`commerce`…). O terceiro segmento é o texto fixo “SQL”. Depois de `USE`, `SET search_path` ou `USE CATALOG`, nada muda, embora a sessão por aba preserve esse estado.
- **Comportamento de referência:**
  - **A e B:** seletor de banco/schema ativo no editor (em B, com atalho próprio).
  - **C:** não documentado.
  - Se o seletor acompanha um `USE` digitado: não documentado em nenhuma delas.
- **Diferença identificada:** O indicador de contexto é estático e às vezes inventado.
- **Por que o comportamento atual é inferior:** O usuário confia no rótulo para saber onde vai rodar um DML, e o rótulo pode estar errado.
- **Comportamento recomendado:** Mostrar o banco/schema da sessão (consultado após cada run ou acompanhado pelo driver) ou retirar os segmentos que não são reais.
- **Evidência no projeto:** `src/app/workspace.rs` — `editor_panel`, `Workspace::new` (`database`), `set_connection`.
- **Confiança:** Confirmado no projeto; acompanhamento do `USE` pelas referências: Necessita validação.

### N018 — Format reformata o script inteiro mesmo havendo seleção
- **Prioridade:** Média
- **Aprovação:** Aprovada (4 de novembro de 2026)
- **Status:** Implementada (4 de novembro de 2026; alterações locais não commitadas). Com seleção, Format formata só o trecho selecionado (`sql::format_range`); sem seleção, o script inteiro. O tooltip do botão diz isso.
- **Funcionalidade:** Formatação
- **Comportamento atual:** O botão Format sempre usa `editor.text()` inteiro e ignora `selected_range`.
- **Comportamento de referência:**
  - **A:** oferece formatar só o trecho selecionado.
  - **B:** a documentação instrui a selecionar o SQL e então formatar.
  - **C:** não documentado.
- **Diferença identificada:** Não existe escopo de formatação.
- **Por que o comportamento atual é inferior:** Formatar uma consulta de um script longo bagunça todo o resto (agravado por N019 e N010).
- **Comportamento recomendado:** Com seleção, formatar só a seleção.
- **Evidência no projeto:** `src/app/workspace.rs` — botão `format`.
- **Confiança:** Confirmado no projeto; referência documentada em A e B.

### N019 — Format apaga linhas em branco e junta instruções DML na mesma linha
- **Prioridade:** Média
- **Aprovação:** Aprovada (4 de novembro de 2026)
- **Status:** Implementada (4 de novembro de 2026; alterações locais não commitadas). `sql::format` formata cada instrução isoladamente (pelos limites de `sql::statements`) e mantém como estava o texto entre elas: linhas em branco, comentários (inclusive o de fim de linha após o `;`) e linhas `DELIMITER`. Toda instrução continua começando numa linha própria; uma instrução que o sqruff quebra em várias linhas ainda empurra as seguintes para baixo. Uma instrução que não formata fica como estava (aviso de formatação parcial).
- **Funcionalidade:** Formatação
- **Comportamento atual:** `format` reconstrói o texto a partir dos tokens e só quebra linha antes de `SELECT`, `FROM`, `WHERE` etc. A separação original some: `INSERT …;\n\nDELETE …;` vira `INSERT …; DELETE …` numa linha só. Comentários de fim de linha passam para a linha seguinte.
- **Comportamento de referência:** Em A, o formatador tem uma opção que controla se as quebras de linha originais são mantidas. Nas outras duas, não documentado.
- **Diferença identificada:** A formatação é destrutiva para o layout que separa as instruções.
- **Por que o comportamento atual é inferior:** O script fica ilegível, e os botões do gutter (um por linha inicial de instrução) e a escolha de `Ctrl+Enter` mudam de posição.
- **Comportamento recomendado:** Formatar cada instrução isoladamente e preservar linhas em branco e comentários entre elas.
- **Evidência no projeto:** `src/sql.rs` — `format`.
- **Confiança:** Confirmado no projeto; referência documentada em A.

### N020 — Uma consulta salva reaberta fica desligada da biblioteca
- **Prioridade:** Média
- **Aprovação:** Aprovada (4 de novembro de 2026)
- **Status:** Implementada (4 de novembro de 2026; alterações locais não commitadas). Abrir uma consulta salva cria a aba já ligada a ela (`saved_id` e `saved_sql`); se uma aba já a edita, ela é mostrada em vez de outra cópia. O vínculo é guardado no `workspace.state` e sobrevive ao reinício.
- **Funcionalidade:** Biblioteca
- **Comportamento atual:** Clicar numa consulta salva chama `add_query`, que cria uma aba com `saved_sql = None`. O ícone mostra “Save query” (não salva). Editar e fechar avisa “The SQL is not in the saved queries”.
- **Comportamento de referência:** Princípio: a aba sabe de onde veio e mostra corretamente se está salva. Em A, scripts são arquivos, e editar altera o próprio script (Provável).
- **Diferença identificada:** A aba não sabe de onde veio.
- **Por que o comportamento atual é inferior:** O estado mostrado é falso, e o usuário não sabe se está editando a consulta salva ou uma cópia.
- **Comportamento recomendado:** Abrir com `saved_sql` preenchido (vínculo pela identidade da consulta).
- **Evidência no projeto:** `src/app/mod.rs` — `sidebar_content` → `add_query`; `src/app/workspace.rs` — `saved_sql`.
- **Confiança:** Confirmado.

### N021 — O botão de salvar consulta remove a consulta quando clicado de novo
- **Prioridade:** Média
- **Aprovação:** Aprovada (4 de novembro de 2026)
- **Status:** Implementada (4 de novembro de 2026; alterações locais não commitadas). O botão só salva ou atualiza (“This query is already saved.” quando nada mudou); remover fica num botão de cada item da lista da biblioteca, com confirmação. A aba que tinha a consulta aberta volta a ser rascunho, e fechá-la pergunta antes. A mensagem passou a ser “Query saved in the library.”.
- **Funcionalidade:** Biblioteca
- **Comportamento atual:** O ícone de marcador alterna: com o SQL igual ao salvo, o clique **remove** da biblioteca (“Query removed from saved queries”), sem confirmação e sem desfazer. A mensagem do primeiro save diz “Query saved in this session.”, embora a biblioteca já seja persistida em `workspace.state`.
- **Comportamento de referência:** Princípio: salvar é idempotente e excluir é uma ação separada.
- **Diferença identificada:** Salvar e excluir estão no mesmo controle.
- **Por que o comportamento atual é inferior:** Clicar duas vezes em “salvar” apaga o trabalho; o texto da mensagem engana sobre a persistência.
- **Comportamento recomendado:** O botão só salva ou atualiza; remover fica na própria lista, com confirmação ou desfazer. Corrigir a mensagem.
- **Evidência no projeto:** `src/app/workspace.rs` — `save-query` em `toolbar`; `src/app/mod.rs` — `WorkspaceEvent::Saved`.
- **Confiança:** Confirmado.

### N022 — Consultas salvas colidem pelo título (Backlog: N018)
- **Prioridade:** Média
- **Aprovação:** Aprovada (4 de novembro de 2026)
- **Status:** Implementada (4 de novembro de 2026; alterações locais não commitadas). Cada consulta salva tem um `id` numérico estável no `workspace.state` (`next_id` guarda o próximo); o título é só rótulo. Estados antigos continuam legíveis: consultas sem `id` ganham um, e a aba é ligada à consulta pelo título, conexão e SQL, como antes. É um formato JSON em `settings`, não uma mudança do schema SQLite.
- **Funcionalidade:** Biblioteca
- **Comportamento atual:** A identidade é `(title, connection)`, e `retain` apaga a homônima. O título é o da aba (“Query 3”, nome da tabela), então duas abas `orders` da mesma conexão se sobrescrevem ao salvar.
- **Comportamento de referência:** Princípio: identidade estável, independente do nome exibido.
- **Diferença identificada:** O nome exibido funciona como chave.
- **Por que o comportamento atual é inferior:** Perda silenciosa e durável de uma consulta salva.
- **Comportamento recomendado:** ID estável; o nome é só rótulo.
- **Evidência no projeto:** `src/app/mod.rs` — `WorkspaceEvent::Saved` (`retain`); `src/workspace_state.rs` — `QueryEntry`.
- **Confiança:** Confirmado.

### N023 — O histórico mostra o título da aba, não o SQL, sem horário nem status
- **Prioridade:** Média
- **Aprovação:** Aprovada (4 de novembro de 2026)
- **Status:** Implementada (4 de novembro de 2026; alterações locais não commitadas). Cada entrada mostra o início do SQL, o status (ícone), a conexão, há quanto tempo, a duração e o desfecho, com busca por SQL, título ou conexão. A lista da biblioteca e do histórico passou a ser virtualizada. Desvio: o horário é relativo (“5 min ago”), como em Messages, porque o horário local exige uma biblioteca de datas (decisão de dependência).
- **Funcionalidade:** Histórico
- **Comportamento atual:** Cada entrada mostra o título da aba de origem e a conexão. Todas as instruções executadas na “Query 2” aparecem como linhas idênticas “Query 2”. Não há horário, duração, status nem busca.
- **Comportamento de referência:** As três mostram a instrução executada:
  - **A:** lista as instruções, com busca e exclusão, e insere a escolhida no editor.
  - **B:** registra horário, duração, linhas e erro de cada execução.
  - **C:** guarda as instruções enviadas, por conexão, e insere a escolhida numa nova aba de editor.
- **Diferença identificada:** A lista identifica a aba, não a execução.
- **Por que o comportamento atual é inferior:** Para encontrar uma consulta é preciso abrir entrada por entrada.
- **Comportamento recomendado:** Mostrar o início do SQL, o horário e o status, com busca.
- **Evidência no projeto:** `src/app/mod.rs` — `sidebar_content`; `src/workspace_state.rs` — `record`.
- **Confiança:** Confirmado no projeto; referência documentada nas três ferramentas.

### N024 — O histórico ignora execuções que falharam (Backlog: N035)
- **Prioridade:** Média
- **Aprovação:** Aprovada (4 de novembro de 2026)
- **Status:** Implementada (4 de novembro de 2026; alterações locais não commitadas). Toda instrução que termina entra no histórico, com status (sucesso, falha ou cancelada), desfecho e duração, pelo mesmo ponto que alimenta Messages. COMMIT e ROLLBACK dos botões da toolbar ficam só em Messages.
- **Funcionalidade:** Histórico
- **Comportamento atual:** Só `WorkspaceEvent::Executed`, emitido por instrução concluída, entra no histórico. A instrução que falhou nunca é registrada.
- **Comportamento de referência:**
  - **A:** guarda todas as instruções executadas no console.
  - **B:** registra também os erros.
  - **C:** não documenta se as falhas entram.
- **Diferença identificada:** O histórico guarda apenas o que deu certo.
- **Por que o comportamento atual é inferior:** Some justamente o SQL que se quer reabrir para corrigir.
- **Comportamento recomendado:** Registrar toda tentativa, com o status.
- **Evidência no projeto:** `src/app/workspace.rs` — `execute_queries` (emit dentro do laço de `completed`).
- **Confiança:** Confirmado no projeto; referência documentada em A e B.

### N025 — A palette e os títulos de aba não mostram o schema
- **Prioridade:** Média
- **Aprovação:** Aprovada (4 de novembro de 2026)
- **Status:** Implementada (4 de novembro de 2026; alterações locais não commitadas). A palette mostra o caminho do objeto esmaecido após o nome, e a aba aberta para uma tabela se chama `schema.tabela` (`explorer::qualified_title`; no MySQL, `banco.tabela`).
- **Funcionalidade:** Navegação
- **Comportamento atual:** Cada resultado da palette mostra o nome do objeto e o nome da conexão; o título da aba aberta é só o nome da tabela. Duas `orders` em schemas diferentes aparecem iguais.
- **Comportamento de referência:** Princípio: objetos homônimos precisam ser distinguíveis onde são escolhidos.
- **Diferença identificada:** O caminho existe (`ObjectPath`), mas não aparece.
- **Por que o comportamento atual é inferior:** Fica fácil abrir a tabela errada, e as abas ficam indistinguíveis.
- **Comportamento recomendado:** Mostrar `schema.tabela` (ou o caminho completo esmaecido) na palette e no tooltip ou título da aba.
- **Evidência no projeto:** `src/app/mod.rs` — `command_palette`; `src/app/connections.rs` — `open_relation` (título = último segmento), `search_cache`.
- **Confiança:** Confirmado.

### N026 — O botão Explain não executa EXPLAIN (Backlog: N024)
- **Prioridade:** Média
- **Aprovação:** Aprovada (4 de novembro de 2026)
- **Status:** Implementada (4 de novembro de 2026; alterações locais não commitadas). Escolhida a alternativa real: Explain executa `EXPLAIN <instrução>` da instrução no cursor (ou da selecionada) num novo result tab, pelo `SqlDialect` da conexão (`sql::explain`), sem mudar drivers nem contratos. Desvio: só consultas e alterações de dados (`SELECT`, `WITH`, `VALUES`, `TABLE`, `INSERT`, `UPDATE`, `DELETE`, `MERGE`, `REPLACE`) são explicadas, nunca com `ANALYZE`, para que a instrução não rode; sem conexão, o botão explica que precisa de um servidor. O assistente simulado não é mais usado pelo botão. `docs/security.md` passou a citar essa ação explícita como SQL gerado pela aplicação; vale a revisão do mantenedor.
- **Funcionalidade:** Explain
- **Comportamento atual:** Chama `ask("Explain this query")` do assistente simulado, que devolve texto fixo do exemplo de vendas, inclusive em conexões reais.
- **Comportamento de referência:** Princípio: a ação mostra o plano real do banco. A e B têm plano de execução real (Provável; não conferido nesta auditoria).
- **Diferença identificada:** Uma ação real da toolbar tem efeito simulado.
- **Por que o comportamento atual é inferior:** Promete um plano que não existe.
- **Comportamento recomendado:** Executar o `EXPLAIN` do dialeto, ou retirar ou renomear o botão enquanto for simulado.
- **Evidência no projeto:** `src/app/workspace.rs` — botão `explain`, `ask`.
- **Confiança:** Confirmado.

### N027 — “Insert into editor” substitui todo o SQL (Backlog: N025)
- **Prioridade:** Média
- **Aprovação:** Aprovada (4 de novembro de 2026)
- **Status:** Implementada (4 de novembro de 2026; alterações locais não commitadas). “Insert at the cursor” insere a sugestão no cursor ou no lugar da seleção, e Ctrl+Z desfaz; o resto do SQL fica.
- **Funcionalidade:** Assistente
- **Comportamento atual:** Usa `replace_all`, sem confirmação.
- **Comportamento de referência:** Princípio: “inserir” acrescenta no cursor ou na seleção; não substitui.
- **Diferença identificada:** O rótulo diz inserir; o efeito é substituir.
- **Por que o comportamento atual é inferior:** O SQL que não estava salvo é sobrescrito.
- **Comportamento recomendado:** Inserir no cursor, ou rotular “Replace editor” e pedir confirmação.
- **Evidência no projeto:** `src/app/workspace.rs` — `apply-ai`.
- **Confiança:** Confirmado.

### N028 — O teste de conexão não pode ser cancelado pelo próprio botão (Backlog: N029)
- **Prioridade:** Média
- **Aprovação:** Aprovada (4 de novembro de 2026)
- **Status:** Implementada (4 de novembro de 2026; alterações locais não commitadas). Durante o teste o botão vira **Cancel test**, cancela o teste e mostra o tempo decorrido depois de 2 s.
- **Funcionalidade:** Diálogo de conexão
- **Comportamento atual:** O botão vira “Testing…” e fica inerte, sem tempo decorrido. Só cancela editando um campo ou fechando o diálogo. No Databricks, o teste pode durar até 90 s.
- **Comportamento de referência:** Em A, é possível cancelar inclusive a abertura da conexão. A regra do próprio Jykre (`docs/ux.md`) pede tempo e cancelamento para o que passa de 2 s.
- **Diferença identificada:** Run vira Cancel; Test não.
- **Por que o comportamento atual é inferior:** O usuário fica preso esperando.
- **Comportamento recomendado:** O botão vira “Cancel test” e mostra o tempo depois de 2 s.
- **Evidência no projeto:** `src/app/connections.rs` — `test_connection`, `test-connection` no rodapé.
- **Confiança:** Confirmado no projeto; referência documentada em A.

### N029 — Salvar a conexão grava perfil e preferências em escritas separadas (Backlog: N030)
- **Prioridade:** Média
- **Aprovação:** Aprovada (4 de novembro de 2026)
- **Status:** Implementada (4 de novembro de 2026; alterações locais não commitadas). Perfil, limite de linhas, escolha de metadados e horas de revalidação são gravados numa só transação (`ConfigRepository::save_with_settings`, nova API do `jykre-storage`, sem mudança de schema). Desvio: a senha continua sendo gravada no cofre antes da transação (o cofre não participa dela); se a transação falha, a senha de uma conexão nova é removida, como antes.
- **Funcionalidade:** Persistência
- **Comportamento atual:** `repository.save` (incrementa a revisão), depois `set_row_limit`, depois `set_metadata_scope`. Se uma escrita posterior falhar, a UI mostra erro com o perfil já alterado, e uma nova tentativa usa a revisão antiga.
- **Comportamento de referência:** Princípio: uma configuração é gravada de forma atômica.
- **Diferença identificada:** Três escritas e um único feedback.
- **Por que o comportamento atual é inferior:** Estado pela metade e falha enganosa; a nova tentativa tende a dar conflito.
- **Comportamento recomendado:** Uma transação.
- **Evidência no projeto:** `src/connection_store.rs` — `save_to`.
- **Confiança:** Confirmado.

### N030 — O status do gutter sobrevive a mudanças de espaço dentro de literais (Backlog: N032)
- **Prioridade:** Média
- **Aprovação:** Aprovada (4 de novembro de 2026)
- **Status:** Implementada (4 de novembro de 2026; alterações locais não commitadas). `normalized_statements` colapsa só o espaço entre tokens; literais, identificadores entre aspas e comentários ficam como estão.
- **Funcionalidade:** Gutter
- **Comportamento atual:** `normalized_statements` aplica `split_whitespace` ao texto inteiro, então `SELECT 'a b'` e `SELECT 'a  b'` contam como a mesma instrução.
- **Comportamento de referência:** Princípio: o indicador é invalidado quando o texto executável muda.
- **Diferença identificada:** A normalização de layout atinge literais.
- **Por que o comportamento atual é inferior:** Mostra ✓ numa instrução que nunca rodou.
- **Comportamento recomendado:** Colapsar espaço apenas entre tokens.
- **Evidência no projeto:** `src/sql.rs` — `normalized_statements`; `src/query.rs` — `sync`.
- **Confiança:** Confirmado.

### N031 — Cópias grandes rodam na thread da UI, sem confirmação (Backlog: N033)
- **Prioridade:** Média
- **Aprovação:** Aprovada (4 de novembro de 2026)
- **Status:** Parcial (4 de novembro de 2026; alterações locais não commitadas). O tamanho é estimado pelo comprimento das células antes de gerar o texto (`grid::selection_bytes`, `grid::csv_bytes`), e acima de 50 MiB a cópia da seleção (`Ctrl C`, menu) e “Copy this page as CSV” pedem confirmação. Bloqueado: gerar o texto fora da thread da UI. O resultado é compartilhado como `Rc<ResultData>`, como registra o [ADR-0012](adr/0012-result-grid-as-a-cached-entity.md), e não pode ir para outra thread; a mudança proposta é compartilhá-lo como `Arc<ResultData>` (ou gerar a cópia no runtime a partir de um `Arc`), o que pede um novo ADR ou a aprovação do mantenedor. O desvio de `docs/results.md` foi atualizado. Atualização (4 de novembro de 2026): a exportação para arquivo trouxe o [ADR-0022](adr/0022-result-pages-shared-across-threads.md) (proposto, decisão aprovada pelo mantenedor), e as páginas passaram a ser `Arc<ResultData>`; gerar a cópia fora da thread da UI deixou de estar bloqueado, mas ainda não foi feito.
- **Funcionalidade:** Clipboard
- **Comportamento atual:** “Copy this page as CSV” e `Ctrl+C` materializam o texto no thread da UI e escrevem direto no clipboard. É um desvio registrado em `docs/results.md`.
- **Comportamento de referência:** Princípio, e a regra do próprio Jykre: confirmar acima de 50 MiB e copiar fora da thread da UI.
- **Diferença identificada:** Não há limite operacional.
- **Por que o comportamento atual é inferior:** A janela pode congelar.
- **Comportamento recomendado:** Estimar o tamanho, confirmar acima de 50 MiB e gerar fora da UI.
- **Evidência no projeto:** `src/app/result_grid.rs` — `copy_selection`; `src/app/workspace.rs` — `export-results`.
- **Confiança:** Confirmado.

### N032 — A execução longa não mostra tempo decorrido (Backlog: N036)
- **Prioridade:** Média
- **Aprovação:** Aprovada (4 de novembro de 2026)
- **Status:** Implementada (4 de novembro de 2026; alterações locais não commitadas). O rodapé mostra “Running… 12.4 s” depois de 2 s (o tooltip cita o timeout da instrução) e, quando o cancelamento passa de 5 s, “Still cancelling…” com o tempo de espera e o que acontece em seguida. O tempo vem de `QueryRun::running_for` e `cancelling_for`; o rodapé é redesenhado a cada segundo só enquanto há run ativo, e a região de status anuncia o estado, não o relógio.
- **Funcionalidade:** Progresso
- **Comportamento atual:** O rodapé mostra “Running…” ou “Cancelling…”. `RunState::Running { started_at }` existe, mas não é exibido. O timeout é escolhido por aba (até 1 h).
- **Comportamento de referência:** A mostra o progresso da execução e avisa quando um cancelamento fica parado. A regra do próprio Jykre pede tempo decorrido acima de 2 s.
- **Diferença identificada:** O relógio está no estado e não na interface.
- **Por que o comportamento atual é inferior:** Não se sabe quanto falta para o timeout nem se o cancelamento travou.
- **Comportamento recomendado:** Mostrar o tempo decorrido depois de 2 s e avisar quando o Cancelling demora.
- **Evidência no projeto:** `src/app/workspace.rs` — `result_footer`; `src/query.rs`.
- **Confiança:** Confirmado no projeto; referência documentada em A.

### N033 — A palette mostra resultados antigos enquanto a nova busca corre (Backlog: N034)
- **Prioridade:** Baixa
- **Aprovação:** Aprovada (4 de novembro de 2026)
- **Status:** Implementada (4 de novembro de 2026; alterações locais não commitadas). Com resultados antigos na lista, a palette mostra “Searching…” com spinner no cabeçalho e esmaece os itens até a busca nova chegar.
- **Funcionalidade:** Busca
- **Comportamento atual:** “Searching…” só aparece com a lista vazia; com matches antigos, nada indica que a busca está pendente.
- **Comportamento de referência:** Princípio: um estado pendente é visível.
- **Diferença identificada:** A lista antiga parece corresponder ao texto novo.
- **Por que o comportamento atual é inferior:** Enter pode abrir um match antigo.
- **Comportamento recomendado:** Um indicador de busca pendente mesmo com resultados na lista.
- **Evidência no projeto:** `src/app/connections.rs` — `search_cache`; `src/app/mod.rs` — `command_palette`.
- **Confiança:** Confirmado.

### N034 — Abrir o mesmo objeto de novo cria uma aba duplicada
- **Prioridade:** Baixa
- **Aprovação:** Aprovada (4 de novembro de 2026)
- **Status:** Implementada (4 de novembro de 2026; alterações locais não commitadas). A aba guarda o objeto para o qual foi aberta (também no `workspace.state`); abrir de novo a mesma tabela da mesma conexão (explorer ou palette) mostra essa aba. O menu do nó ganhou **New query for table in another tab** para abrir outra.
- **Funcionalidade:** Abas / navegação
- **Comportamento atual:** Cada duplo clique numa tabela ou entrada da palette cria uma aba nova com o mesmo título.
- **Comportamento de referência:**
  - **C:** abrir em aba nova exige um comando explícito (“abrir em nova aba”), o que indica que a abertura normal não duplica.
  - **A:** reaproveita a aba do objeto já aberto (Provável).
  - **B:** não documentado.
- **Diferença identificada:** Não há reaproveitamento.
- **Por que o comportamento atual é inferior:** A barra se enche de abas iguais (e alimenta N022).
- **Comportamento recomendado:** Focar a aba existente do mesmo objeto, com uma ação explícita “abrir outra”.
- **Evidência no projeto:** `src/app/mod.rs` — `add_query`; `src/app/connections.rs` — `open_relation`.
- **Confiança:** Confirmado no projeto; referência Provável.

### N035 — O rodapé conta as linhas da página ignorando o filtro
- **Prioridade:** Baixa
- **Aprovação:** Aprovada (4 de novembro de 2026)
- **Status:** Implementada (4 de novembro de 2026; alterações locais não commitadas). Com o filtro aberto o rodapé diz “3 of 1,000 rows” (ou “3 of rows 1,001–2,000” numa página que não é a única), pelo número que a grade informa (`ResultGridEvent::RowsShown`, `result_tabs::rows_label`).
- **Funcionalidade:** Grade / rodapé
- **Comportamento atual:** Com o filtro ativo, o rodapé continua “1.000 rows” enquanto a grade mostra 3.
- **Comportamento de referência:** Princípio: rodapé e grade contam a mesma coisa.
- **Diferença identificada:** O rodapé e a grade discordam.
- **Por que o comportamento atual é inferior:** O usuário não sabe quantas linhas casaram.
- **Comportamento recomendado:** “3 of 1,000 rows” enquanto o filtro estiver ativo.
- **Evidência no projeto:** `src/app/workspace.rs` — `page_summary` (`page.data.row_count()`).
- **Confiança:** Confirmado.

### N036 — A grade não aceita Home, End, PageUp, PageDown nem Ctrl+Home/End
- **Prioridade:** Baixa
- **Aprovação:** Aprovada (4 de novembro de 2026)
- **Status:** Implementada (4 de novembro de 2026; alterações locais não commitadas). Home/End (linha), Ctrl+Home/End (página) e PageUp/PageDown (uma tela de linhas), com Shift estendendo a seleção (`CellSelection::jumped`). Desvio: as teclas continuam tratadas no `on_key_down` da grade, como as setas, e não como ações do keymap (desvio já registrado em `docs/ui-guidelines.md`).
- **Funcionalidade:** Grade / teclado
- **Comportamento atual:** `key_down` trata apenas setas, Shift+setas, `Ctrl+A`, `Ctrl+C` e Esc.
- **Comportamento de referência:** Convenção de grades e planilhas: navegar por página e ir aos extremos. Não conferido na documentação das três ferramentas.
- **Diferença identificada:** A navegação por teclado existe, mas é incompleta.
- **Por que o comportamento atual é inferior:** Percorrer 1.000 linhas exige segurar a seta.
- **Comportamento recomendado:** Adicionar Home/End (linha), Ctrl+Home/End (grade) e PageUp/PageDown, com Shift estendendo a seleção.
- **Evidência no projeto:** `src/app/result_grid.rs` — `key_down`.
- **Confiança:** Confirmado no projeto; referência Provável.

### N037 — Alternar abas pelo teclado exige foco na barra de abas
- **Prioridade:** Baixa
- **Aprovação:** Aprovada (4 de novembro de 2026)
- **Status:** Implementada (4 de novembro de 2026; alterações locais não commitadas). Ações `NextTab` e `PreviousTab` no keymap: `Ctrl+Tab`/`Ctrl+Shift+Tab` e `Ctrl+PageDown`/`Ctrl+PageUp`, de qualquer lugar da janela (inclusive o editor); o teclado vai para o editor da aba escolhida.
- **Funcionalidade:** Abas / teclado
- **Comportamento atual:** As setas, Home e End trocam de aba apenas com o foco na barra. Não há atalho global de próxima ou anterior aba a partir do editor.
- **Comportamento de referência:** Convenção de editores com abas: atalho global de próxima e anterior aba. Não conferido na documentação das três ferramentas.
- **Diferença identificada:** Trocar de aba custa várias tabulações.
- **Por que o comportamento atual é inferior:** É lento para quem trabalha só no teclado.
- **Comportamento recomendado:** Atalhos globais de próxima e anterior aba, no keymap.
- **Evidência no projeto:** `src/grouped_tabs/mod.rs` — `navigate`; `src/app/mod.rs` — `capture_key_down`.
- **Confiança:** Confirmado no projeto; referência Provável.

### N038 — O clique do meio não fecha a aba
- **Prioridade:** Baixa
- **Aprovação:** Aprovada (4 de novembro de 2026)
- **Status:** Implementada (4 de novembro de 2026; alterações locais não commitadas). O clique do meio numa aba segue o mesmo caminho do X (`CloseRequested`, com as mesmas confirmações).
- **Funcionalidade:** Abas
- **Comportamento atual:** Só o X ou `Ctrl+W`/`Ctrl+F4` fecham a aba.
- **Comportamento de referência:** Convenção de aplicações com abas (IDEs e navegadores). Não conferido na documentação das três ferramentas.
- **Diferença identificada:** Falta um gesto padrão na interação de fechar que já existe.
- **Por que o comportamento atual é inferior:** É preciso mirar no X, que só aparece no hover nas abas não selecionadas.
- **Comportamento recomendado:** Clique do meio chama `CloseRequested` (com as mesmas confirmações).
- **Evidência no projeto:** `src/grouped_tabs/mod.rs` (não há handler de `MouseButton::Middle`).
- **Confiança:** Confirmado no projeto; referência Provável.

### N039 — Nova consulta já vem com `SELECT 1;`
- **Prioridade:** Baixa
- **Aprovação:** Aprovada (4 de novembro de 2026)
- **Status:** Implementada (4 de novembro de 2026; alterações locais não commitadas). Nova consulta começa vazia, com o placeholder “Write SQL here. Ctrl+Enter runs the statement at the cursor.”, com ou sem conexão.
- **Funcionalidade:** Abas
- **Comportamento atual:** `new_query` preenche `SELECT 1;` (ou `SELECT * FROM orders LIMIT 100;` sem conexão).
- **Comportamento de referência:** Um editor novo começa vazio (Provável; não conferido na documentação).
- **Diferença identificada:** Há conteúdo a apagar antes de digitar.
- **Por que o comportamento atual é inferior:** Toda consulta nova começa com seleção e remoção.
- **Comportamento recomendado:** Editor vazio, com placeholder.
- **Evidência no projeto:** `src/app/mod.rs` — `new_query`.
- **Confiança:** Confirmado no projeto; referência Provável.

### N040 — “Load more objects” aparece no fim da conexão, longe do nó que estende
- **Prioridade:** Baixa
- **Aprovação:** Aprovada (4 de novembro de 2026)
- **Status:** Implementada (já atendida pelo código atual; conferido em 4 de novembro de 2026, sem mudança). Desde `aca08bc` o explorer é uma lista em que “Load more objects” é a última linha dos filhos do próprio nó (`Tree::push_children`), recuada como eles, e não um bloco no fim da conexão.
- **Funcionalidade:** Explorer
- **Comportamento atual:** Os botões de mais páginas de todos os nós aparecem juntos no fim da árvore da conexão, identificados só pelo nome do pai.
- **Comportamento de referência:** Princípio: o controle de continuação fica junto da lista que estende.
- **Diferença identificada:** O controle está separado da lista que estende.
- **Por que o comportamento atual é inferior:** Em schemas longos é difícil perceber que a lista está incompleta e achar o botão.
- **Comportamento recomendado:** Renderizar o botão como último filho do nó correspondente.
- **Evidência no projeto:** `src/app/connections.rs` — `live_connection_tree` (`metadata_more`).
- **Confiança:** Confirmado.

### N041 — O clique simples age diferente conforme o tipo de nó
- **Prioridade:** Baixa
- **Aprovação:** Aprovada (4 de novembro de 2026)
- **Status:** Implementada (4 de novembro de 2026; alterações locais não commitadas). Uma regra só: o clique seleciona; duplo clique ou Enter abre a tabela, ou expande/recolhe os demais nós; todo nó com filhos (inclusive bancos, schemas e a própria conexão) tem chevron; →/← expandem e recolhem também na conexão.
- **Funcionalidade:** Explorer
- **Comportamento atual:** Em banco ou schema, o clique simples seleciona **e** expande ou recolhe. Em tabela, só seleciona (as colunas abrem pela seta à esquerda). Na conexão, o clique expande ou recolhe sem selecionar.
- **Comportamento de referência:** Consistência interna. As ferramentas variam: em B, o efeito do duplo clique (abrir ou expandir) é configurável. Nenhuma documenta três regras diferentes na mesma árvore.
- **Diferença identificada:** Três regras de clique na mesma árvore.
- **Por que o comportamento atual é inferior:** Selecionar um schema para usar o menu de contexto o recolhe.
- **Comportamento recomendado:** Uma única regra: o clique simples seleciona; a expansão fica com o chevron, o duplo clique e →/←.
- **Evidência no projeto:** `src/app/connections.rs` — `on_click` dos nós (`expandable`) e da conexão.
- **Confiança:** Confirmado.

### N042 — Tempo de execução sempre exibido em ms
- **Prioridade:** Baixa
- **Aprovação:** Aprovada (4 de novembro de 2026)
- **Status:** Implementada (4 de novembro de 2026; alterações locais não commitadas). `cell_format::duration` mostra ms abaixo de 1 s, segundos com uma casa (separador decimal do idioma), min e s, h e min; usado no rodapé, em Messages, no histórico e no teste de conexão.
- **Funcionalidade:** Rodapé
- **Comportamento atual:** `format!("{} ms", elapsed_ms)`, então um run de 3 minutos aparece como “183421 ms”.
- **Comportamento de referência:** Princípio de legibilidade: a duração é exibida na escala adequada.
- **Diferença identificada:** Falta de escala.
- **Por que o comportamento atual é inferior:** Execuções longas ficam difíceis de ler.
- **Comportamento recomendado:** Formatar em s, min ou h acima de 1 s.
- **Evidência no projeto:** `src/app/workspace.rs` — `result_footer`.
- **Confiança:** Confirmado.

### N043 — O grupo de abas some ao fechar a última aba e volta no fim da barra (Backlog: N037)
- **Prioridade:** Baixa
- **Aprovação:** Reprovada (4 de novembro de 2026)
- **Status:** Descartada. O grupo deve sumir ao perder a última aba, e reaparecer no fim da barra ao abrir uma nova aba da conexão é o comportamento esperado. Sem mudança de código.
- **Funcionalidade:** Grupos de abas
- **Comportamento atual:** `model.close` remove o grupo vazio e `ensure_group` o recria no fim da barra.
- **Comportamento de referência:** Princípio: a posição dos elementos da interface é estável.
- **Diferença identificada:** A ordem da barra muda conforme o histórico de abas.
- **Por que o comportamento atual é inferior:** A posição dos grupos fica imprevisível.
- **Comportamento recomendado:** Manter o grupo na posição, ou ocultar grupos vazios sempre.
- **Evidência no projeto:** `src/grouped_tabs/model.rs` — `close`; `src/app/mod.rs` — `ensure_group`.
- **Confiança:** Confirmado.

### N044 — O explorer constrói todos os nós visíveis a cada render (Backlog: N038)
- **Prioridade:** Baixa
- **Aprovação:** Aprovada (4 de novembro de 2026)
- **Status:** Implementada (já atendida pelo código atual; conferido em 4 de novembro de 2026, sem mudança). Desde `aca08bc` a árvore é uma lista achatada e virtualizada (`ListState` com `explorer_entries`, reconstruída só quando a árvore, a expansão ou a busca mudam), e `docs/performance.md` não lista mais o desvio. Não foi medida nesta alteração.
- **Funcionalidade:** Explorer / desempenho
- **Comportamento atual:** `.children(objects…)` sobre a coleção visível, com IDs montados por `format!` em cada nó. É um desvio registrado em `docs/performance.md`.
- **Comportamento de referência:** A regra do próprio Jykre: coleções longas são virtualizadas.
- **Diferença identificada:** A grade é virtualizada; a árvore não.
- **Por que o comportamento atual é inferior:** Foco, busca e refresh degradam em catálogos grandes.
- **Comportamento recomendado:** Lista achatada e virtualizada.
- **Evidência no projeto:** `src/app/connections.rs` — `live_connection_tree`.
- **Confiança:** Confirmado (não medido).

### N045 — O tamanho da fonte pode ser gravado fora de ordem (Backlog: N039)
- **Prioridade:** Baixa
- **Aprovação:** Aprovada (4 de novembro de 2026)
- **Status:** Implementada (4 de novembro de 2026; alterações locais não commitadas). Debounce de 300 ms mais geração: cada gravação recebe um ticket (`connection_store::FONT_SIZE_WRITES`), as gravações não correm ao mesmo tempo, e uma superada por outra mais nova é descartada.
- **Funcionalidade:** Preferências
- **Comportamento atual:** Cada `Ctrl +/−` dispara uma escrita sem geração; erros aparecem no aviso do Explorer.
- **Comportamento de referência:** Princípio: a última alteração vence.
- **Diferença identificada:** A persistência não é monotônica.
- **Por que o comportamento atual é inferior:** A UI mostra 18 px e a aplicação reinicia com 16.
- **Comportamento recomendado:** Debounce ou geração.
- **Evidência no projeto:** `src/app/connections.rs` — `persist_font_size`.
- **Confiança:** Confirmado.

### N046 — A seta de ordenação do demo só inverte a ordem (Backlog: N040)
- **Prioridade:** Baixa
- **Aprovação:** Aprovada (4 de novembro de 2026)
- **Status:** Implementada (4 de novembro de 2026; alterações locais não commitadas). Escolhida a ordenação de verdade, em todos os resultados (não só no demo): clicar no cabeçalho ordena as linhas da página pelos valores da coluna, crescente, decrescente e de volta à ordem do servidor (`grid::ColumnSort`). Números são comparados exatamente como texto decimal, inclusive na forma com expoente (`1.5e-05`, como o `float8` imprime), nunca em ponto flutuante; NULLs ficam no fim; valores iguais mantêm a ordem do servidor. Desvio: como o filtro, ordena só a página em memória, e a ordenação roda na thread da UI ao reconstruir a `GridView` (benchmark em release: cerca de 5 ms para 100.000 linhas já em ordem, dentro do orçamento de 50 ms de `docs/performance.md`; não medida no aplicativo).
- **Funcionalidade:** Grade (demo)
- **Comportamento atual:** Na terceira coluna do resultado demo, o clique só alterna `descending` (`reverse`); em resultados reais o cabeçalho não ordena.
- **Comportamento de referência:** Princípio: uma seta no cabeçalho ordena pelos valores da coluna.
- **Diferença identificada:** A affordance diz ordenar; a operação inverte.
- **Por que o comportamento atual é inferior:** O demo ensina um gesto que não existe no uso real.
- **Comportamento recomendado:** Remover a seta do demo ou implementar ordenação de verdade.
- **Evidência no projeto:** `src/app/result_grid.rs` — cabeçalho (`index == 2 && self.sales`).
- **Confiança:** Confirmado.

### N047 — A gravação no cofre não tem timeout e pode prender o diálogo (Backlog: N041)
- **Prioridade:** Baixa
- **Aprovação:** Aprovada (4 de novembro de 2026)
- **Status:** Implementada (4 de novembro de 2026; alterações locais não commitadas). Toda leitura, gravação e remoção no cofre feita por `connection_store` tem timeout de 10 s (`VAULT_TIMEOUT`); passado ele, o cofre conta como indisponível: a senha vale só para a sessão, “Remember password” é salvo desligado com aviso, e o diálogo sai do estado “Saving”. Registrado na tabela de timeouts de `docs/reliability.md`.
- **Funcionalidade:** Cofre / diálogo
- **Comportamento atual:** O diálogo não fecha durante `connection_saving`, e `secrets.set` não tem timeout.
- **Comportamento de referência:** Princípio, e a regra do próprio Jykre: toda operação externa tem timeout.
- **Diferença identificada:** A espera pode ser ilimitada.
- **Por que o comportamento atual é inferior:** Um keyring travado deixa o modal sem saída.
- **Comportamento recomendado:** Timeout e aviso.
- **Evidência no projeto:** `src/app/connections.rs` — `close_connection_dialog`; `crates/secrets/src/lib.rs`.
- **Confiança:** Provável (depende do backend do sistema operacional).

---

## 3. Tabela de prioridades

Na coluna **Referência**, as letras indicam as ferramentas cuja documentação sustenta o comportamento de referência; “P” é princípio ou regra do próprio projeto, e “~” indica referência provável, não conferida.

| ID | Funcionalidade | Divergência | Impacto | Referência | Prioridade | Aprovação |
| --- | --- | --- | --- | --- | --- | --- |
| N001 | Execução | Atalho de executar cancela | DML longo cancelado por engano | A, C | Alta | Aprovada · Implementada |
| N002 | Resultados | “Run query again” roda o cursor | Pode executar outra instrução | P | Alta | Aprovada · Implementada |
| N003 | Explorer | Abrir tabela não mostra dados | Passo extra em toda inspeção | A, B, C | Alta | Aprovada · Implementada |
| N004 | Autenticação | Senha não lembrada não é pedida | Falha e edição de perfil a cada sessão | A, B | Alta | Aprovada · Implementada |
| N005 | Conexões | Salvar perfil derruba runs e resultados | Perda de trabalho em andamento | P | Alta | Aprovada · Implementada |
| N006 | Biblioteca/histórico | Abre na primeira conexão | SQL no banco errado | A, C | Alta | Aprovada · Implementada |
| N007 | Execução | Aba sem conexão roda o demo | Dados fictícios em uso real | P | Alta | Aprovada · Implementada |
| N008 | Transações | Transação invisível | Perda ou persistência ilusória | B, A~ | Alta | Aprovada · Implementada |
| N009 | SQL | Splitter ignora o dialeto | Rotinas fatiadas | B, A~ | Alta | Aprovada · Implementada |
| N010 | Formatação | Literais MySQL e caret | Significado e posição alterados | P | Alta | Aprovada · Implementada |
| N011 | Explorer | `LIMIT 100` fixo | Paginação inútil | A, C | Média | Aprovada · Implementada |
| N012 | Result tabs | Sobrescreve o tab “mantido” | Resultado perdido | A, B | Média | Aprovada · Implementada |
| N013 | Editor | Cursor fora executa a vizinha | Execução inesperada | A, B | Média | Aprovada · Implementada |
| N014 | Grade | Filtro só na página | “Nada encontrado” falso | B~, A~ | Média | Aprovada · Implementada |
| N015 | Grade | Seleção e scroll não são por tab | Comparação difícil | B | Média | Aprovada · Implementada |
| N016 | Mensagens | Sem log de execução | Desfecho de DML some | B, A~ | Média | Aprovada · Implementada |
| N017 | Contexto | Caminho estático ou inventado | Rótulo errado | A, B | Média | Aprovada · Implementada |
| N018 | Formatação | Ignora seleção | Script inteiro alterado | A, B | Média | Aprovada · Implementada |
| N019 | Formatação | Junta instruções | Script ilegível | A | Média | Aprovada · Implementada |
| N020 | Biblioteca | Reabertura desligada | Estado de salvo falso | P | Média | Aprovada · Implementada |
| N021 | Biblioteca | Salvar remove no segundo clique | Exclusão acidental | P | Média | Aprovada · Implementada |
| N022 | Biblioteca | Colisão por título | Sobrescrita silenciosa | P | Média | Aprovada · Implementada |
| N023 | Histórico | Título em vez de SQL | Difícil encontrar | A, B, C | Média | Aprovada · Implementada |
| N024 | Histórico | Falhas não entram | SQL a corrigir some | A, B | Média | Aprovada · Implementada |
| N025 | Navegação | Sem schema | Tabela errada | P | Média | Aprovada · Implementada |
| N026 | Explain | Simulado | Promessa falsa | P, A~, B~ | Média | Aprovada · Implementada |
| N027 | Assistente | Inserir substitui | SQL sobrescrito | P | Média | Aprovada · Implementada |
| N028 | Conexão | Teste sem cancelamento | Usuário preso | A, P | Média | Aprovada · Implementada |
| N029 | Persistência | Save não atômico | Estado pela metade | P | Média | Aprovada · Implementada |
| N030 | Gutter | Whitespace em literal | ✓ falso | P | Média | Aprovada · Implementada |
| N031 | Clipboard | Cópia grande na UI | Congelamento | P | Média | Aprovada · Parcial |
| N032 | Progresso | Sem tempo decorrido | Timeout invisível | A, P | Média | Aprovada · Implementada |
| N033 | Busca | Pendente invisível | Abre match velho | P | Baixa | Aprovada · Implementada |
| N034 | Abas | Abas duplicadas | Barra poluída | C, A~ | Baixa | Aprovada · Implementada |
| N035 | Rodapé | Contagem ignora filtro | Números discordantes | P | Baixa | Aprovada · Implementada |
| N036 | Grade | Sem Home/End/PgUp/PgDn | Navegação lenta | ~ | Baixa | Aprovada · Implementada |
| N037 | Abas | Troca só com foco na barra | Teclado lento | ~ | Baixa | Aprovada · Implementada |
| N038 | Abas | Sem clique do meio | Fechar exige mira | ~ | Baixa | Aprovada · Implementada |
| N039 | Abas | Nova consulta pré-preenchida | Atrito | ~ | Baixa | Aprovada · Implementada |
| N040 | Explorer | “Load more” longe do nó | Lista incompleta passa despercebida | P | Baixa | Aprovada · Implementada |
| N041 | Explorer | Clique simples inconsistente | Recolhe sem querer | P | Baixa | Aprovada · Implementada |
| N042 | Rodapé | Duração só em ms | Leitura ruim | P | Baixa | Aprovada · Implementada |
| N043 | Grupos | Grupo some e volta no fim | Ordem instável | P | Baixa | Reprovada |
| N044 | Explorer | Árvore não virtualizada | Lentidão em escala | P | Baixa | Aprovada · Implementada |
| N045 | Preferências | Fonte gravada fora de ordem | Valor errado ao reiniciar | P | Baixa | Aprovada · Implementada |
| N046 | Demo | Seta inverte em vez de ordenar | Gesto falso | P | Baixa | Aprovada · Implementada |
| N047 | Cofre | Sem timeout | Modal preso | P | Baixa | Aprovada · Implementada |

---

## 4. Comportamentos superiores do projeto (preservar)

- **Cancelamento que bloqueia novo run.** `Cancelling` impede um segundo run na mesma aba e o botão mostra “Stopping…”, então nunca há duas sessões disputando o mesmo trabalho. B, por exemplo, alerta na própria documentação que execuções em paralelo podem gerar deadlocks.
- **Paginação pelo cursor aberto, sem reexecutar.** A página seguinte nunca roda a instrução de novo, o que é seguro para consultas não determinísticas ou caras. Quando o cursor fecha, a interface diz isso.
- **Sessão por aba com avisos honestos.** A aplicação avisa quando a sessão foi reaberta ou encerrada e que o contexto (schema, temporárias) recomeçou.
- **Erro do servidor completo.** Título por código, mensagem original, SQLSTATE, posição, *Copy details* e “Go to the position” sem selecionar texto. Isso evita que a próxima tecla apague SQL.
- **Mover uma aba entre conexões pergunta antes e preserva o SQL.**
- **Fechar uma aba com SQL fora da biblioteca pergunta antes**; o workspace é restaurado entre sessões.
- **Senha não migra entre servidores** (`keeps_password`): uma senha vazia só mantém a anterior para o mesmo driver, host, porta e usuário.
- **Apresentação fiel de valores.** NULL distinto de vazio e de espaços (`(empty)`, `␠ N spaces`), decimais exatos, nenhuma conversão de fuso, quebras de linha como `↵`.
- **Introspecção sob escolha do usuário**, publicada de forma atômica e com fila de refresh (que absorve nós já cobertos).
- **Explorer e palette pelo teclado.** O clique seleciona, o duplo clique ou Enter abre, e as setas navegam a árvore e a palette.
- **O clique direito fora da seleção seleciona a célula antes**, então o menu de contexto age sobre o que está sob o ponteiro.
- **Timeout por aba visível na toolbar.**
- **Gutter por instrução que acompanha o servidor** (Pending → Running → Success/Error).

---

## 5. Principais inconsistências transversais

1. **Controles que alternam entre uma ação e o seu oposto.** O atalho de executar também cancela (N001) e o botão de salvar também remove (N021). Repetir o gesto, o erro mais comum de interface, desfaz o que o usuário acabou de pedir.
2. **Identidade por nome.** A conexão das abas, do histórico e da biblioteca é localizada pelo nome, e a consulta salva pelo título. Daí vêm N006, N007, N020, N022 e N034. Um ID estável (`ConnectionId`, ID da consulta) resolve a família inteira.
3. **Resultado e contexto separados da origem.** O result tab guarda `tab.sql`, mas as ações sobre ele usam o editor (N002), o alvo de substituição é “o último criado” e não o visível (N012), e o desfecho morre com o tab (N016). O indicador de contexto também não acompanha a sessão (N017).
4. **Código de demonstração em caminhos reais.** Abas sem conexão rodam o demo (N007), Explain e o assistente são simulados (N026, N027), o banco do cabeçalho vem de uma heurística do demo (N017) e o Chart só funciona no demo.
5. **Mudanças de estado sem diálogo.** Salvar o perfil cancela runs (N005), a transação é revertida ao fechar (N008), a senha ausente vira erro (N004), e a substituição de resultados e as falhas no histórico acontecem sem registro (N016, N024).
6. **Atalhos fora do keymap.** O desvio documentado em `docs/ui-guidelines.md` (atalhos tratados à mão em `capture_key_down`/`on_key_down`) explica a navegação incompleta na grade e nas abas (N036, N037) e a falta de um atalho próprio de cancelar (N001).

---

## 6. Sequência recomendada de correções

A sequência abaixo ordena todos os itens, mas só os **Aprovados** entram em desenvolvimento; os Pendentes ficam no lugar indicado quando forem aprovados.

1. **Segurança do que é executado: N001, N002, N013, N007, N006.** São todos casos de executar algo diferente do que o usuário pediu, ou no lugar errado. São correções pequenas e de efeito imediato.
2. **Conexão e sessão: N004, N005, N008.** O prompt de senha elimina o caminho que hoje força o save do perfil; o N005 deixa de derrubar trabalho; a transação visível fecha o maior risco de perda silenciosa.
3. **Resultados como objetos próprios: N012 (fixar), N016 (log), N015 (estado por tab), N014/N035 (escopo do filtro).**
4. **Abrir tabela de verdade: N003 e N011**, junto com N025 e N034 na navegação.
5. **Biblioteca e histórico com ID estável: N022, N020, N021, N023, N024.** Fazer junto com o item 2 das inconsistências transversais.
6. **SQL enviado e formatado: N009, N010, N018, N019, N030.** O splitter por dialeto vem antes da formatação, que deveria reutilizar o mesmo tokenizer.
7. **Feedback e acabamento: N017, N026–N029, N031–N033, N036–N047.**

---

## Limitações

- A aplicação não foi executada. Itens marcados como “Provável” na parte do projeto (scroll em N015, N047) dependem de comportamento em tempo de execução do GPUI ou do sistema operacional.
- A documentação das três ferramentas cobre os casos de forma desigual. C tem a documentação mais enxuta, e por isso aparece menos como referência. “Não documentado” não equivale a “se comporta diferente”.
- Os itens marcados com “~” ou “Provável” na referência não foram conferidos na documentação consultada e devem ser validados antes de orientar uma decisão de produto.

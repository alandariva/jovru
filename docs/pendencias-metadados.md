# Pendências do subsistema de metadados

Itens da auditoria do subsistema de metadados que **ainda não foram
implementados**, ou foram implementados só em parte, em `release/0.1.0`
(3 de novembro de 2026, atualizado depois da fase 2). As referências de
mercado aparecem com nomes genéricos: "IDEs de banco de dados",
"gerenciadores de banco de dados" e "ferramentas de referência".

Os números `MET-xxx` são os da auditoria original, para rastreio. Os
achados já fechados aparecem só no resumo.

## Resumo

| Situação | Quantidade | IDs |
| --- | ---: | --- |
| Fechados | 9 | MET-001, MET-007, MET-011, MET-012, MET-013, MET-015, MET-018, MET-020, MET-021 |
| Parciais (ainda falta uma parte) | 7 | MET-002, MET-003, MET-004, MET-005, MET-006, MET-016, MET-017 |
| Ainda abertos | 6 | MET-008, MET-009, MET-010, MET-014, MET-019, MET-022 |
| **Pendências neste arquivo** | **13** | os parciais e os abertos |

**O que já foi entregue**

- **MET-001.** Seleção obrigatória do que introspectar, em todos os drivers
  ([ADR-0015](adr/0015-introspect-only-what-the-user-chooses.md)):
  - descoberta de namespaces nível a nível (`Driver::discover_namespaces`);
  - regras persistidas por conexão (`connection.<id>.metadata_scope`);
  - leitura e publicação por namespace, com barra de progresso e tempo
    restante;
  - remoção do que deixou de ser escolhido (`MetadataRepository::retain`).
- **MET-007.** O namespace padrão de cada conexão (`Driver::default_namespaces`)
  vem marcado no diálogo, com o selo **Padrão**, enquanto nada foi escolhido.
- **Parte de MET-005 e MET-006**
  ([ADR-0016](adr/0016-keys-defaults-and-relation-kinds.md)):
  - tipos exatos, defaults, comentários, posição na chave primária e
    chaves estrangeiras;
  - views materializadas e partições;
  - no PostgreSQL, a leitura passou para `pg_catalog`.
- **Diálogo de conexão** dividido nas seções Conexão, Metadados e Opções,
  com rodapé fixo.
- **MET-015.** Uma fila de refresh por conexão
  (`jykre::refresh_queue::RefreshQueue`):
  - um F5 durante outro refresh entra na fila e aparece como **na fila**;
  - um nó já coberto por outro da fila é ignorado, e um nó mais amplo
    substitui os de dentro;
  - pedir de novo tira o pedido da fila;
  - o lock de escrita do cache só é usado na publicação, e um lock de
    execução separado mantém a recuperação de refreshes interrompidos.
- **MET-018.** O refresh de um nó avisa quando salvou mas não conseguiu
  exibir a árvore. Salvar a conexão só interrompe os refreshes depois de o
  salvamento dar certo (N028 e N031 da auditoria comportamental).
- **MET-020.** `backend::metadata` e `backend::merge_metadata` foram
  removidos, e `MetadataRepository::all` passou a existir só nos testes. Os
  testes do SQLite já cobriam o que os testes da versão em memória
  verificavam.
- **Parte de MET-017.** `RefreshStatus.state` é o enum
  `jykre_storage::RefreshState`, sem mudar o texto gravado.
- **MET-011.** O cache de cada conexão fica aberto enquanto a aplicação
  roda (registro em `backend`). Ele só é aberto, e verificado com
  `quick_check`, no startup (uma vez por conexão, não mais duas), depois de
  a identidade do perfil mudar ou depois de uma leitura com erro.
- **MET-012.**
  - Expandir lê só a página pedida (`backend::cached_children`), com
    paginação por cursor (`PageCursor`).
  - A versão do cache e as páginas vêm de uma única transação. Uma página
    de outra versão faz a árvore ser relida, em vez de pedir "expanda de
    novo".
  - O merge usa um `HashSet` por nó.
- **MET-013.**
  - O explorer é uma lista virtualizada (`list` do GPUI) e só monta as
    linhas na tela.
  - As linhas vêm de `jykre::explorer::Tree`, recalculadas só quando a
    árvore muda.
  - A árvore guarda os nós expandidos.
  - Os IDs das linhas são hashes do caminho estrutural.
  - O texto de atualidade é montado só ao abrir o menu.
  - A ordem de tipos é fixa: tabelas, views e depois views materializadas.
- **MET-021.** Os nós expandidos ficam em `connection.<id>.explorer`, ao
  lado do estado do workspace em `settings`, e voltam no startup, lidos do
  cache de uma vez.
- **Parte de MET-016.** Um selo de idade ("há 3 dias") ao lado de cada
  namespace escolhido.

**O que mais pesa agora**

1. O editor ainda não usa os metadados (**MET-008**).
2. O contexto de execução de cada aba continua invisível (**MET-009**).
3. Nada acontece depois de um DDL (**MET-010**).

---

## P1: Integração com o editor

### MET-008: o editor não usa os metadados

- **Situação atual:**
  - `Workspace` configura só o realce léxico (`SqlHighlighter`), sem
    autocomplete, hover ou ir para a definição.
  - O componente de editor (`gpui-kit` 0.9) já oferece `CompletionProvider`,
    `HoverProvider` e `DefinitionProvider`.
- **Referência:** as IDEs de banco de dados sugerem tabelas, colunas, aliases
  e CTEs conforme o contexto SQL, e mostram tipo e comentário no hover.
- **O que falta:**
  - Um índice de nomes por conexão, imutável e versionado, montado fora da
    thread de UI a partir do cache.
  - Um provider que use o tokenizer (`jykre::sql::tokens`) e o statement no
    cursor para:
    - sugerir relações depois de `FROM`/`JOIN`;
    - sugerir colunas em `alias.`;
    - resolver nomes qualificados.
  - Usar sempre a conexão e o contexto da própria aba.
  - Pedir em segundo plano as colunas de uma relação ainda sem colunas no
    cache, sem bloquear a digitação.
  - Hover com tipo, nulabilidade, default, chave e comentário (os dados já
    existem desde a ADR-0016).
  - Ir para a definição, levando ao nó do explorer.
- **Dependências e riscos:**
  - O provider usa tipos `lsp_types`, que hoje só existem como dependência
    transitiva. Torná-los dependência direta exige aprovação.
  - O alvo de uma FK pode estar fora da seleção e, portanto, fora do cache.
- **Critérios de aceitação:**
  - Uma aba da conexão A nunca sugere objetos da conexão B.
  - Depois de `FROM orders o`, `o.` sugere as colunas de `orders`.
  - Uma relação sem colunas no cache mostra "carregando" e é completada sem
    travar o editor.

### MET-009: contexto de execução por aba invisível e sem controle

- **Situação atual:**
  - `ExecutionOptions.context` vai sempre vazio.
  - O cabeçalho da aba mostra o `database` da configuração, não o schema
    corrente da sessão.
  - O PostgreSQL não tem `search_path` configurável, e a sugestão de padrão
    (MET-007) usa `public`, não o `search_path` real.
  - Se `context.schema` for usado, o driver PostgreSQL executa
    `SET search_path` a cada statement e descarta `$user` e `public`.
- **Referência:** os gerenciadores de banco de dados têm um seletor de
  catálogo e schema ativos por editor, que acompanha os `USE` executados.
- **O que falta:**
  - Um seletor de contexto no cabeçalho da aba: schema no PostgreSQL,
    database no MySQL, catálogo e schema no Databricks.
  - Aplicar o contexto na sessão uma vez, só quando ele muda.
  - Refletir `USE`/`SET` bem-sucedidos.
  - Repor o contexto quando a sessão for reaberta.
- **Critérios de aceitação:**
  - Depois de `USE sales`, o cabeçalho mostra `sales`.
  - Trocar o schema no seletor muda onde um `SELECT` não qualificado roda.
  - Um `SET search_path` do usuário não é sobrescrito a cada statement.

### MET-010: nenhuma atualização depois de DDL

- **Situação atual:** o workspace não chama nenhuma operação de metadados.
  Depois de `CREATE TABLE`, a tabela só aparece com F5 no schema.
- **Referência:** as IDEs de banco de dados sincronizam a estrutura depois
  de um DDL executado no editor.
- **O que falta:**
  - Classificar o statement pelo tokenizer (`CREATE`, `ALTER`, `DROP`,
    `RENAME`, `COMMENT`) e identificar o alvo.
  - Depois do sucesso, enfileirar a releitura do schema afetado, ou do
    contexto ativo quando o nome não for qualificado, com debounce.
  - Respeitar a seleção: um schema fora dela não é lido.
- **Critério de aceitação:** `CREATE TABLE x` bem-sucedido faz `x` aparecer
  no explorer em até 2 s, sem ação manual.

## P1: Desempenho do cache e do explorer

### MET-014: busca sem índice, só ASCII e frágil

- **Situação atual:**
  - `instr(lower(name))` percorre a tabela inteira, e o `lower` do SQLite só
    converte ASCII.
  - Os ancestrais são buscados em N+1.
  - As conexões são consultadas em sequência, e uma falha derruba a busca
    de todas.
  - A palette filtra tabelas **depois** do `LIMIT 200`.
- **O que falta:**
  - Uma coluna de nome normalizado, com índice (FTS5 trigram, se a medição
    exigir).
  - Filtrar por tipo no SQL.
  - Erros por conexão, sem afetar as outras.
  - Consultas em paralelo.
- **Critérios de aceitação:**
  - Buscar `ação` encontra `AÇÃO`.
  - Um cache quebrado não impede a busca nas outras conexões.
  - Buscar `id` mostra a tabela `orders` mesmo que existam mais de 200
    colunas `id`.

## P2: Introspecção e refresh

### MET-002 (parcial): Databricks em escala

- **Já feito:** cada schema é lido e publicado sozinho, com limite de tempo
  por leitura. Uma falha não derruba os outros.
- **O que falta:**
  - Paralelismo limitado (de 4 a 8 requisições ao mesmo tempo). Hoje os
    namespaces são lidos em sequência. O índice `one_running_refresh` e o
    lock de execução só permitem um refresh ativo por cache: ler em
    paralelo e publicar em sequência evita mudar o schema, e remover o
    índice exige migração.
  - Listar só nomes com `table-summaries` ou `tables?omit_columns=true`, e
    ler colunas sob demanda.
  - Evitar o GET do catálogo e do schema que cada leitura de schema faz
    antes de listar as tabelas.
  - Deixar o catálogo `system` fora de **Tudo** até ele ser marcado
    explicitamente.
- **Critério de aceitação:** 20 catálogos × 50 schemas, num mock com
  latência, terminam em menos de 60 s com paralelismo 4.

### MET-003 (parcial): editar o perfil apaga o cache

- **Já feito:** salvar com o cache vazio lê só a seleção, nunca a conexão
  inteira.
- **O que falta:**
  - Hoje a identidade do cache é a configuração inteira. Trocar compute,
    `defaultCatalog`, `defaultSchema` ou `sslMode` apaga o cache e força uma
    releitura.
  - Cada driver deve definir quais opções identificam o catálogo. Isso muda
    o contrato e exige aprovação.
- **Critério de aceitação:** trocar o warehouse de uma conexão Databricks
  mantém o cache.

### MET-004 (parcial): níveis de introspecção

- **Já feito:**
  - O PostgreSQL lê o `pg_catalog`, um schema por vez.
  - O MySQL lê um database por vez.
- **O que falta:**
  - Níveis de leitura: só nomes de relações primeiro, colunas e chaves
    depois, ou só ao expandir. Hoje cada namespace é lido completo.
  - No MySQL, as colunas ainda vêm na mesma consulta das relações.
  - Medir o tempo por schema em catálogos grandes (por exemplo, um fixture
    sintético com 200 schemas).
- **Critério de aceitação:** expandir um schema com 5 mil tabelas mostra os
  nomes antes de as colunas chegarem.

### MET-005 (parcial): cobertura de metadados

- **Já feito:** tipos exatos, defaults, comentários, primary key, foreign
  keys, views materializadas e partições.
- **O que falta, lido sob demanda (inspeção) e não em todo refresh:**
  - índices, unique e check constraints;
  - funções e procedures com seus parâmetros;
  - sequences e triggers;
  - tipos customizados e enums do PostgreSQL;
  - defaults de coluna no Databricks (não são lidos hoje);
  - DDL e fonte de views e rotinas, só por ação explícita.
- **Critério de aceitação:** uma ação **Inspecionar** numa tabela mostra
  índices e constraints sem que o refresh normal os leia.

### MET-006 (parcial): especificidades do PostgreSQL

- **Já feito:**
  - `pg_catalog` e views materializadas.
  - Partições sob a tabela raiz.
  - Schemas TOAST e temporários excluídos.
- **O que falta:**
  - Uma partição em schema diferente do da tabela raiz ainda aparece como
    tabela comum.
  - Listar os outros databases do servidor, como opção. Cada database exige
    uma conexão própria.

### MET-016 (parcial): revalidação ao abrir a conexão

- **Já feito:**
  - Barra de progresso com o tempo restante ao ler a seleção.
  - Selo de idade ao lado de cada namespace escolhido.
- **O que falta:**
  - Revalidação opcional ao abrir a conexão, só dos namespaces escolhidos e
    só acima de um limite de idade configurável. Desligada por padrão.
  - Sem polling.

## P3: Ajustes e dívidas

### MET-017 (parcial): última publicação de um refresh parcial

- **Já feito:** o enum `RefreshState` no `jykre-storage`, sem mudar o texto
  gravado.
- **Situação atual:** um refresh parcial não atualiza `last_success_at`,
  então aparece "Última atualização bem-sucedida: nenhuma".
- **O que falta:** registrar a última publicação separada do último sucesso
  completo. Isso pede uma coluna nova em `scope_status`, ou seja, o primeiro
  passo de migração do cache, que exige ADR e aprovação.

### MET-019: permissões

- **Situação atual:**
  - PostgreSQL e MySQL omitem em silêncio o que o usuário não pode ver.
  - O Databricks mantém as subárvores sem permissão, marcadas como
    "parcial".
- **O que falta:**
  - Mostrar a data da última leitura boa numa subárvore protegida.
  - Um nó "sem acesso".
  - Avaliar `include_browse` no Unity Catalog (não verificado).

### MET-022: o cache guarda o caminho três vezes

- **Situação atual:** cada objeto grava `path_key` (JSON em hexadecimal),
  `path_json` e `object_json`, que repete o caminho.
- **Medido uma vez:** o cache sintético do benchmark (500.101 objetos, só nomes,
  sem colunas) ocupa 2,3 GiB entre arquivo e WAL logo depois de publicado.
  A medição ainda não separa o WAL do arquivo depois do checkpoint, nem
  quanto cabe a cada coluna.
- **O que falta:** separar essas partes antes de decidir. A mudança exige
  migração e ADR.

### Diálogo de conexão

- **Lista desatualizada:** se as configurações mudarem depois de a lista ser
  carregada, o diálogo só avisa. Ele poderia recarregar sozinho, ou impedir
  que uma seleção feita para outro servidor seja salva.
- **Tempo total:** com **Tudo**, o tempo total é parecido com o de antes,
  porque os namespaces são lidos em sequência (ver MET-002).

---

## Validações pendentes

| Item | Situação |
| --- | --- |
| Verificação visual do diálogo de conexão (seções, rodapé fixo, foco, teclado, altura com o formulário do Databricks) | Não feita |
| Verificação visual do explorer (ícones por tipo, tipo da coluna, tooltips, partições, selo **Padrão**, barra de progresso e tempo restante) | Não feita |
| Databricks com CLI autenticado e workspace de teste: descoberta de catálogos e schemas, `table_constraints` na listagem de tabelas, views materializadas | Não feita (só mock HTTP) |
| Tempo da nova consulta `pg_catalog` em um catálogo grande | Não medido |
| Frame do explorer com 5 mil nós expandidos (critério do MET-013) | Não medido; o explorer não foi aberto nesta fase |
| Abertura a frio do cache com 500 mil objetos (MET-011) | Cerca de 700 ms (benchmark, release), por causa do `quick_check`. Acontece uma vez por conexão, no startup e fora da thread de UI; expandir não abre mais o arquivo |
| ADR-0015 e ADR-0016 | *Proposed*; aguardam aceitação do mantenedor |

## Ordem sugerida

1. **MET-009 e MET-010.** Contexto por aba e atualização depois de DDL, que
   preparam o editor.
2. **MET-008.** Autocomplete, hover e ir para a definição.
3. **MET-002, MET-003 e MET-004.** Escala da introspecção.
4. **MET-005 e MET-014,** depois o restante de P3.

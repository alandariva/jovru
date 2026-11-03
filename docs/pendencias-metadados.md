# Pendências do subsistema de metadados

Itens da auditoria do subsistema de metadados que **ainda não foram
implementados**, ou foram implementados só em parte, em `release/0.1.0`
(3 de novembro de 2026, atualizado depois das fases 3 e 4). As referências de
mercado aparecem com nomes genéricos: "IDEs de banco de dados",
"gerenciadores de banco de dados" e "ferramentas de referência".

Os números `MET-xxx` são os da auditoria original, para rastreio. Os
achados já fechados aparecem só no resumo.

## Resumo

| Situação | Quantidade | IDs |
| --- | ---: | --- |
| Fechados | 11 | MET-001, MET-007, MET-008, MET-010, MET-011, MET-012, MET-013, MET-015, MET-018, MET-020, MET-021 |
| Parciais (ainda falta uma parte) | 8 | MET-002, MET-003, MET-004, MET-005, MET-006, MET-009, MET-016, MET-017 |
| Ainda abertos | 3 | MET-014, MET-019, MET-022 |
| **Pendências neste arquivo** | **11** | os parciais e os abertos |

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
- **MET-008.** O editor usa os metadados da própria conexão da aba
  (`jykre::completion`, providers do `gpui-component`):
  - sugere relações, CTEs e namespaces depois de `FROM`/`JOIN`;
  - sugere colunas depois de `alias.` e, sem qualificador, as das relações
    do statement;
  - colunas ainda não lidas aparecem como "Carregando colunas…" e são
    lidas em segundo plano;
  - o hover mostra tipo, nulabilidade, chave, default e comentário;
  - `Ctrl`-clique leva ao nó do explorer.
  - O índice de nomes é imutável, versionado pelo cache e montado fora da
    thread de UI. `lsp-types` virou dependência direta (decisão D2).
- **MET-010.** Um `CREATE`, `ALTER`, `DROP`, `RENAME` ou `COMMENT`
  bem-sucedido enfileira, uns 300 ms depois, a releitura dos namespaces
  escolhidos que ele tocou (`jykre::ddl`). Nomes não qualificados contam no
  contexto de antes e no de depois da execução.

**O que mais pesa agora**

1. Escala da introspecção (**MET-002**, **MET-003**, **MET-004**), com o
   contrato da [ADR-0017](adr/0017-driver-metadata-contract-batch.md).
2. Inspeção sob demanda (**MET-005**) e permissões (**MET-019**).
3. Busca Unicode e indexada (**MET-014**), com a migração da
   [ADR-0018](adr/0018-metadata-cache-migrations.md).

---

## P1: Integração com o editor

### MET-009 (parcial): `search_path` configurável no PostgreSQL

- **Já feito** (opção A da decisão D3, `Session::context`):
  - seletor de contexto no cabeçalho da aba;
  - contexto aplicado na sessão só quando muda;
  - `USE`/`SET` refletidos pelo que a sessão informa depois de cada
    execução;
  - contexto reposto quando a sessão reabre e guardado com a aba.
  - O `USE` do MySQL, que falhava no protocolo de prepared statements
    (erro 1295), passou a rodar pelo protocolo de texto.
- **O que falta:** um `search_path` configurável na conexão PostgreSQL, que
  também sirva de padrão sugerido no diálogo (MET-007) em vez de `public`.

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

1. **MET-002, MET-003 e MET-004.** Escala da introspecção, com a
   [ADR-0017](adr/0017-driver-metadata-contract-batch.md).
2. **MET-005, MET-006 e MET-019.** Inspeção, PostgreSQL e permissões.
3. **MET-014 e MET-017,** com a migração da
   [ADR-0018](adr/0018-metadata-cache-migrations.md), depois o restante.

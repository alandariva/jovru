# Pendências do subsistema de metadados

Itens da auditoria do subsistema de metadados, em `release/0.1.0`
(3 de novembro de 2026, atualizado depois das fases 1 a 7). As referências de
mercado aparecem com nomes genéricos: "IDEs de banco de dados",
"gerenciadores de banco de dados" e "ferramentas de referência".

Os números `MET-xxx` são os da auditoria original, para rastreio.

## Resumo

| Situação | Quantidade | IDs |
| --- | ---: | --- |
| Fechados | 22 | MET-001 a MET-022 |
| **Pendências neste arquivo** | **0** | Só as validações abaixo |

**O que foi entregue**

- **MET-001 e MET-007.** Seleção obrigatória do que introspectar
  ([ADR-0015](adr/0015-introspect-only-what-the-user-chooses.md)), com o
  namespace padrão marcado no diálogo.
- **MET-002.**
  - A leitura da seleção lê até quatro namespaces ao mesmo tempo e publica
    um de cada vez.
  - O Databricks lê um schema só pela listagem das tabelas, sem o GET do
    catálogo e do schema, e pede `omit_columns` na leitura de nomes.
  - O catálogo `system` fica fora de **Tudo** até ser escolhido pelo nome.
  - Um teste com latência simulada (20 schemas, 40 leituras de 40 ms) termina
    bem abaixo do tempo sequencial.
- **MET-003.** Cada driver define a identidade do cache
  (`Driver::cache_identity`):
  - PostgreSQL: servidor, banco e usuário;
  - MySQL: servidor e usuário;
  - Databricks: o perfil do CLI.

  Trocar compute, defaults ou TLS mantém o cache, e o cache de uma versão
  anterior é adotado sem ser esvaziado.
- **MET-004.** Níveis de leitura (`IntrospectionLevel::Names` e `Full`). Um
  namespace lido pela primeira vez mostra os nomes antes das colunas; um já
  em cache é lido só por completo, para as colunas nunca sumirem no meio.
- **MET-005.** Ação **Inspecionar**, lida do servidor sob demanda e nunca
  guardada no cache:
  - em relações: índices, unique, check, triggers;
  - em schemas: funções e procedures com parâmetros, sequences, tipos e
    enums.

  **Mostrar definição** abre o DDL ou a fonte numa aba. No Databricks, os
  defaults de coluna vêm do `type_json`.
- **MET-006.**
  - Uma partição em outro schema aparece sob a tabela raiz, e o preview a lê
    onde ela está (`stored_at`).
  - O perfil PostgreSQL pode oferecer os outros databases do servidor, cada
    um lido por uma conexão própria.
- **MET-008.** Autocomplete, hover e ir para a definição a partir do cache da
  própria aba (`jykre::completion`).
- **MET-009.**
  - Contexto por aba (opção A da decisão D3): a sessão informa o contexto, o
    cabeçalho o mostra e permite trocá-lo.
  - O contexto é aplicado só quando muda e reposto quando a sessão reabre.
  - O PostgreSQL tem `search_path` configurável, que também define o padrão
    sugerido no diálogo.
  - O `USE` do MySQL passou a rodar pelo protocolo de texto.
- **MET-010.** Um DDL bem-sucedido enfileira a releitura dos namespaces
  escolhidos que ele tocou (`jykre::ddl`).
- **MET-011 a MET-013.**
  - O cache fica aberto por conexão.
  - A paginação é por cursor e lê só a página pedida.
  - O explorer é virtualizado, com ordem fixa de tipos e IDs estáveis.
- **MET-014.** Busca Unicode:
  - nome normalizado com índice FTS5 trigram;
  - filtro de tipo no SQL antes do limite;
  - ancestrais lidos numa consulta;
  - conexões consultadas em paralelo, com erro por conexão.
- **MET-015 e MET-018.** Fila de refresh por conexão, lock só na publicação,
  erro de releitura visível e salvar antes de interromper o refresh.
- **MET-016.** Selo de idade por namespace e revalidação opcional ao abrir a
  aplicação, só dos namespaces escolhidos mais velhos que o limite
  configurado em **Opções**. Desligada por padrão, sem polling.
- **MET-017.** Estado do refresh como enum e data da última publicação
  separada do último sucesso completo (passo 2 da migração,
  [ADR-0018](adr/0018-metadata-cache-migrations.md)).
- **MET-019.** Um namespace escolhido sem permissão vira aviso: o selo
  **sem acesso** aparece no nó, a última leitura boa é mantida e o menu
  mostra a data dela.
  - PostgreSQL: `USAGE` no schema.
  - MySQL: `USE`. Para um login sem privilégio global, o MySQL responde
    "acesso negado" também para um database inexistente.
  - Databricks: erros de permissão por catálogo ou schema.
- **MET-020 e MET-021.** Caminhos de publicação sem uso removidos; nós
  expandidos guardados por conexão.
- **MET-022.** Chaves compactas ([ADR-0019](adr/0019-compact-metadata-cache-keys.md),
  passo 3 da migração):
  - a chave é o JSON de cada segmento com `%` e `/` escapados, no lugar do
    hexadecimal;
  - `path_json` saiu, e o `object_json` é gravado sem o caminho, que a
    leitura reconstrói pela chave;
  - o passo roda com chaves estrangeiras desligadas, confere as referências
    antes do commit e compacta o arquivo uma vez.

  Um cache de 500 mil objetos caiu de 892 MB para 461 MB (de 1,5 GB para
  753 MB como um refresh o deixa). A migração leva cerca de 20 s nesse
  tamanho, uma vez, fora da thread de UI.
- **Diálogo de conexão.** Uma escolha nova não pode ser salva com a lista de
  outro servidor: é preciso carregá-la de novo.

## Validações pendentes

| Item | Situação |
| --- | --- |
| Verificação visual: explorer virtualizado, selo de idade, **sem acesso**, **na fila**, painel de inspeção, seletor de contexto, autocomplete e hover, campos novos do diálogo | Não feita (o app não foi aberto nestas fases) |
| Frame do explorer com 5 mil nós expandidos (critério do MET-013) | Não medido |
| Databricks com CLI autenticado e workspace de teste: leitura em paralelo, `omit_columns`, funções, `view_definition`, defaults em `type_json`, `include_browse` | Não feita (só mock HTTP) |
| Tempo da consulta `pg_catalog` em um catálogo grande | Medido em 3/11/2026, PostgreSQL 18 em Docker local, catálogo sintético de 100 schemas × 500 tabelas × 10 colunas, com PK e FK (50 mil tabelas, 500 mil colunas), consultas do driver executadas por `psql`. Um schema completo: 116 a 141 ms. Um schema só com nomes: 60 a 66 ms. Uma tabela: 54 a 63 ms. Foreign keys de um schema: 14 ms. Banco inteiro completo: 5,2 a 6,1 s (timeout de 300 s). Banco inteiro só com nomes: 0,57 s. Foreign keys do banco inteiro: 0,7 a 0,9 s. A leitura de uma tabela custa quase o mesmo que a de um schema, porque o filtro por raiz de partição (`OR rt.name = $2`) não usa índice. Não há orçamento para leituras remotas |
| Busca de 1 ou 2 caracteres com 500 mil objetos | 93 ms depois das chaves compactas, acima do orçamento de 50 ms; registrada como desvio em [Performance](performance.md) |
| Abertura a frio do cache com 500 mil objetos | Cerca de 680 ms, uma vez por conexão no startup, fora da thread de UI |
| ADR-0015 a ADR-0018 | *Proposed*; aguardam aceitação do mantenedor. A ADR-0019 foi aceita em 3/11/2026 |

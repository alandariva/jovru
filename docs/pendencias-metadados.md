# Pendências do subsistema de metadados

Itens da auditoria do subsistema de metadados, em `release/0.1.0`
(3 de novembro de 2026, atualizado depois das fases 1 a 7). As referências de
mercado aparecem com nomes genéricos: "IDEs de banco de dados",
"gerenciadores de banco de dados" e "ferramentas de referência".

Os números `MET-xxx` são os da auditoria original, para rastreio.

## Resumo

| Situação | Quantidade | IDs |
| --- | ---: | --- |
| Fechados | 21 | MET-001 a MET-021 |
| Parcial (proposta aguardando aprovação) | 1 | MET-022 |
| **Pendências neste arquivo** | **1** | MET-022, mais as validações abaixo |

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
- **Diálogo de conexão.** Uma escolha nova não pode ser salva com a lista de
  outro servidor: é preciso carregá-la de novo.

## Pendente

### MET-022 (parcial): o cache guarda o caminho várias vezes

- **Medido:** um cache sintético de 500 mil tabelas ocupa 775 MB, e cerca de
  três quartos disso é o caminho repetido. Ele aparece em:
  - `path_key` em hexadecimal (111 MB);
  - `parent_key` (73,5 MB);
  - `path_json` (57 MB);
  - dentro do `object_json`;
  - nos índices.
- **Proposta:** a [ADR-0019](adr/0019-compact-metadata-cache-keys.md)
  (*Proposed*) prevê chaves compactas e uma só cópia do caminho, com ganho
  estimado de 40 a 50%. A mudança reescreve todas as chaves de todo cache e
  aguarda aprovação.

## Validações pendentes

| Item | Situação |
| --- | --- |
| Verificação visual: explorer virtualizado, selo de idade, **sem acesso**, **na fila**, painel de inspeção, seletor de contexto, autocomplete e hover, campos novos do diálogo | Não feita (o app não foi aberto nestas fases) |
| Frame do explorer com 5 mil nós expandidos (critério do MET-013) | Não medido |
| Databricks com CLI autenticado e workspace de teste: leitura em paralelo, `omit_columns`, funções, `view_definition`, defaults em `type_json`, `include_browse` | Não feita (só mock HTTP) |
| Tempo da consulta `pg_catalog` em um catálogo grande | Não medido |
| Busca de 1 ou 2 caracteres com 500 mil objetos | 114 ms, acima do orçamento de 50 ms; registrada como desvio em [Performance](performance.md) |
| Abertura a frio do cache com 500 mil objetos | Cerca de 700 ms, uma vez por conexão no startup, fora da thread de UI |
| ADR-0015 a ADR-0019 | *Proposed*; aguardam aceitação do mantenedor |

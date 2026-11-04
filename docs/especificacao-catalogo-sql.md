# Especificação — catálogo aberto de dialetos SQL (formato de dados)

Status: **Proposta**, rascunho 1 · 4 de novembro de 2026.

Este documento define o formato de um repositório **separado** que publica, para cada dialeto SQL, as palavras-chave, as funções embutidas e os tipos, com a faixa de versões em que cada entrada vale. Ele foi escrito para virar o `SPEC.md` desse repositório, que ainda não existe. Nada foi criado. O contexto está em [investigacao-editor-sql.md](investigacao-editor-sql.md).

Marcas usadas: **[V]** verificado na fonte citada, **[H]** hipótese a confirmar ao implementar.

---

## 1. Objetivo

Oferecer dados prontos e fáceis de consumir em qualquer linguagem para ferramentas que precisam conhecer um dialeto SQL: autocomplete, highlighting, formatação, hover, aspas em identificadores.

Requisitos que guiam o formato:

1. **Texto simples, sem biblioteca específica**: JSON Lines lido linha a linha.
2. **Incremental**: uma versão nova de um banco só acrescenta ou fecha as linhas do que mudou. Uma microversão sem mudanças altera apenas o manifesto.
3. **Diffs legíveis**: cada mudança aparece como uma linha adicionada ou alterada no Git, e um humano revisa cada PR.
4. **Proveniência e licença por entrada**: todo dado diz de onde veio, e só se distribui texto cuja licença permite.

## 2. Escopo

**Versão 1 do formato**: PostgreSQL, MySQL e Databricks SQL; palavras-chave, funções embutidas (com sobrecargas) e tipos.

**Fora do escopo:**
- Objetos do usuário: tabelas, funções criadas no banco.
- Cópias integrais de manuais.
- Regras de gramática, como quais palavras cabem depois de `FROM`.
- Serviço ou API online: o repositório só publica arquivos.

Operadores e outros dialetos podem entrar depois, sem quebrar o formato (seção 6.1).

## 3. Estrutura do repositório

```text
SPEC.md                    # este documento
LICENSE                    # licença do conteúdo próprio (CC0-1.0, seção 7.3)
schema/                    # JSON Schema de cada tipo de arquivo
  manifest.schema.json
  keyword.schema.json
  function.schema.json
  type.schema.json
postgresql/
  manifest.json
  keywords.jsonl
  functions.jsonl
  types.jsonl
  LICENSE-postgresql       # aviso exigido pelos textos extraídos
mysql/
  manifest.json
  keywords.jsonl
  functions.jsonl
  types.jsonl
databricks/
  manifest.json
  keywords.jsonl
  functions.jsonl
  types.jsonl
  NOTICE-spark             # se houver texto de origem Apache Spark
generator/                 # código que gera e atualiza os dados (fora do formato)
CHANGELOG.md               # correções e mudanças de formato
```

Cada diretório de dialeto é **autocontido**: quem consome só o PostgreSQL lê só `postgresql/`. Dados comuns, como os herdados do Spark pelo Databricks, são copiados pelo gerador, não referenciados entre diretórios.

## 4. Modelo de versões

### 4.1 Esquema de versão por dialeto

| Dialeto | `version_scheme` | Granularidade acompanhada | Exemplo | Motivo |
| --- | --- | --- | --- | --- |
| PostgreSQL | `numeric` | Major | `"18"` | Os arquivos de catálogo são idênticos entre versões menores: `pg_proc.dat`, `kwlist.h` e `pg_type.dat` têm o mesmo blob em 17.0 e 17.11 e em 18.0 e 18.6 [V] |
| MySQL | `numeric` | Release | `"9.7.0"`, `"26.10.0"` | O MySQL muda funcionalidade em releases pontuais |
| Databricks | `date` | Data da observação | `"2026-11-04"` | O Databricks SQL não publica versões do motor e muda continuamente |

**Comparação:**
- `numeric`: componente a componente como inteiros, completando o mais curto com zeros. Assim `"18"` = `[18, 0]` < `"18.6"` = `[18, 6]` < `"19"`.
- `date`: datas ISO `YYYY-MM-DD`, comparadas como texto.

### 4.2 Validade de uma entrada

Toda linha pode ter `since` e `until`. A faixa é **semiaberta**: a linha vale nas versões `t` com `since ≤ t < until`.

- `since` ausente: vale desde a primeira versão de `versions` no manifesto.
- `until` ausente: continua valendo.
- `since` e `until` sempre usam valores que estão em `manifest.versions`.
- Para uma mesma chave, as faixas das linhas **não se sobrepõem**.
- Linhas nunca são apagadas por mudança do banco: uma entrada que deixou de existir ganha `until`.

### 4.3 `versions` e `verified_through`

- `versions`: lista ordenada das versões em que dados foram gerados. Contém a versão inicial acompanhada e, depois, só as versões que **mudaram** algo.
- `verified_through`: a versão mais recente já conferida. O gerador processa as releases em ordem, então **toda** versão menor ou igual a `verified_through` foi conferida, inclusive as que não aparecem em `versions` por não terem mudado nada.

### 4.4 Resolver uma versão pedida

Dada a versão `r` do servidor:

1. `t` = maior versão de `versions` com `t ≤ r`. Exemplo: com `versions = ["14","15","16","17","18"]`, `r = "18.6"` resolve para `"18"`.
2. Se `r` é anterior à primeira versão de `versions`: usar a primeira e avisar "versão anterior às acompanhadas".
3. Se `r` é posterior a `verified_through`: usar a última e avisar "versão ainda não conferida".
4. Sem versão conhecida: usar a última de `versions`.

Uma linha vale para `t` quando `(since ausente ou since ≤ t)` e `(until ausente ou t < until)`.

## 5. Arquivos

### 5.1 Regras gerais

- **Codificação**: UTF-8 sem BOM, fim de linha LF, nova linha no fim do arquivo.
- **`.jsonl`**: um objeto JSON por linha, em **JSON canônico** ([RFC 8785](https://www.rfc-editor.org/rfc/rfc8785), JCS): chaves em ordem lexicográfica, sem espaços. Assim, a mesma informação produz sempre o mesmo texto.
- **Ordenação**: linhas ordenadas pela chave do registro e, para a mesma chave, por `since`.
- **Campos omitidos**: campos com valor padrão (`false`, lista vazia, ausente) não são escritos.
- **Palavras e nomes**: em minúsculas na chave. A grafia de exibição fica com o consumidor.
- **`manifest.json`**: JSON com chaves ordenadas e indentação de 2 espaços, para leitura humana.
- **Evolução**: um campo **novo e opcional** não muda `schema_version`. Remover ou mudar o significado de um campo muda.

Campos comuns a todos os registros:

| Campo | Tipo | Obrigatório | Significado |
| --- | --- | --- | --- |
| `since` | string | não | Seção 4.2 |
| `until` | string | não | Seção 4.2 |
| `source` | string | sim | `id` de uma fonte em `manifest.sources`: de onde o registro foi gerado ou `curated` |
| `note` | string | não | Observação curta escrita pelo projeto |

### 5.2 `manifest.json`

| Campo | Tipo | Significado |
| --- | --- | --- |
| `schema_version` | inteiro | Versão deste formato (1) |
| `dialect` | string | Identificador; igual ao nome do diretório |
| `name` | string | Nome de exibição |
| `version_scheme` | `"numeric"` \| `"date"` | Seção 4.1 |
| `versions` | lista de strings | Seção 4.3 |
| `verified_through` | string | Seção 4.3 |
| `series_components` | inteiro | Quantos componentes da versão formam a "série" usada nas URLs (PostgreSQL 1: `18`; MySQL 2: `8.4`) |
| `doc_base` | string | Modelo de URL da documentação, com `{series}` |
| `files` | objeto | Nome do arquivo de cada tipo de registro |
| `sources` | lista | Fontes (abaixo) |
| `generator` | objeto | `name` e `version` do gerador que escreveu os dados |
| `updated_at` | string | Data ISO da última alteração |

Cada fonte em `sources`:

| Campo | Significado |
| --- | --- |
| `id` | Identificador referenciado por `source` e `summary_source` |
| `title` | Descrição curta |
| `url` | Origem; pode usar `{series}` |
| `license` | Identificador SPDX, `"proprietary"` ou `"facts"` (dados factuais extraídos de um servidor, sem texto) |
| `redistributable` | `true` se **texto** dessa fonte pode ser incluído (`summary`, arquivos de documentação) |
| `notice` | Arquivo de aviso no diretório, quando a licença exige |

Exemplo:

```json
{
  "dialect": "postgresql",
  "doc_base": "https://www.postgresql.org/docs/{series}/",
  "files": {
    "functions": "functions.jsonl",
    "keywords": "keywords.jsonl",
    "types": "types.jsonl"
  },
  "generator": { "name": "catalog-gen", "version": "0.1.0" },
  "name": "PostgreSQL",
  "schema_version": 1,
  "series_components": 1,
  "sources": [
    {
      "id": "kwlist.h",
      "license": "PostgreSQL",
      "notice": "LICENSE-postgresql",
      "redistributable": true,
      "title": "Lista de palavras-chave do parser",
      "url": "https://github.com/postgres/postgres/blob/REL_{series}_STABLE/src/include/parser/kwlist.h"
    },
    {
      "id": "pg_proc.dat",
      "license": "PostgreSQL",
      "notice": "LICENSE-postgresql",
      "redistributable": true,
      "title": "Funções embutidas do catálogo",
      "url": "https://github.com/postgres/postgres/blob/REL_{series}_STABLE/src/include/catalog/pg_proc.dat"
    },
    {
      "id": "curated",
      "license": "CC0-1.0",
      "redistributable": true,
      "title": "Conteúdo escrito pelo projeto"
    }
  ],
  "updated_at": "2026-11-04",
  "verified_through": "18.6",
  "version_scheme": "numeric",
  "versions": ["14", "15", "16", "17", "18"]
}
```

### 5.3 `keywords.jsonl`

Chave: `word`.

| Campo | Tipo | Obrigatório | Significado |
| --- | --- | --- | --- |
| `word` | string | sim | A palavra, em minúsculas |
| `reserved` | booleano | sim | `true` se **não pode** ser usada sem aspas como nome de tabela ou coluna. É o campo que um editor usa para decidir as aspas |
| `category` | string | não | Categoria original do dialeto. PostgreSQL: `unreserved`, `col_name`, `type_func_name`, `reserved`; no PostgreSQL, `reserved` é `true` para `type_func_name` e `reserved` |
| `bare_label` | booleano | não | PostgreSQL: pode ser rótulo de coluna sem `AS` |
| `conditional` | booleano | não | Reservada só em alguns modos (por exemplo, ANSI no Databricks); detalhe em `note` |

Exemplos reais, a partir de `kwlist.h` [V]. `json_table` não existe em `REL_16_0` e aparece em `REL_17_0`:

```json
{"bare_label":true,"category":"col_name","reserved":false,"since":"17","source":"kwlist.h","word":"json_table"}
{"category":"reserved","reserved":true,"source":"kwlist.h","word":"order"}
{"bare_label":true,"category":"reserved","reserved":true,"source":"kwlist.h","word":"select"}
```

(`order` é `AS_LABEL`, então `bare_label` é `false` e não é escrito.)

### 5.4 `functions.jsonl`

Chave: `id`. Uma linha **por sobrecarga**.

| Campo | Tipo | Obrigatório | Significado |
| --- | --- | --- | --- |
| `id` | string | sim | Identidade estável: `nome(tipo,tipo,…)` com os tipos na grafia canônica do dialeto, quando conhecidos (PostgreSQL: `typname` de `pg_proc.dat`). Sem tipos formais: `nome(param,param,…)` com os nomes dos parâmetros da assinatura |
| `name` | string | sim | Nome, em minúsculas |
| `kind` | string | sim | `scalar`, `aggregate`, `window`, `table` ou `procedure` |
| `args` | lista | não | Cada argumento: `name`, `type` (chave em `types.jsonl`), `optional`, `variadic` |
| `returns` | string | não | Tipo de retorno (chave em `types.jsonl`) |
| `returns_set` | booleano | não | Retorna um conjunto de linhas |
| `signature` | string | sim | Texto de exibição da assinatura |
| `summary` | string | não | Resumo de até 200 caracteres |
| `summary_source` | string | se houver `summary` | Fonte do texto; deve ter `redistributable: true` |
| `doc` | string | não | Caminho relativo a `doc_base`, ou URL absoluta |
| `doc_file` | string | não | Documentação longa no repositório; só de fontes redistribuíveis |

Exemplo real, da entrada `oid 1284` de `pg_proc.dat` [V]:

```json
{"args":[{"type":"text"},{"type":"timestamptz"},{"type":"text"}],"doc":"functions-datetime.html","id":"date_trunc(text,timestamptz,text)","kind":"scalar","name":"date_trunc","returns":"timestamptz","signature":"date_trunc(text, timestamp with time zone, text) → timestamp with time zone","source":"pg_proc.dat","summary":"truncate timestamp with time zone to specified units in specified time zone","summary_source":"pg_proc.dat"}
```

### 5.5 `types.jsonl`

Chave: `name`.

| Campo | Tipo | Obrigatório | Significado |
| --- | --- | --- | --- |
| `name` | string | sim | Nome canônico do dialeto (PostgreSQL: `typname`) |
| `display` | string | sim | Grafia SQL de exibição |
| `aliases` | lista de strings | não | Outras grafias aceitas |
| `params` | string | não | Parâmetros aceitos, como `(precision, scale)` |
| `category` | string | não | `numeric`, `string`, `datetime`, `boolean`, `binary`, `json`, `array`, `composite`, `network`, `geometric`, `other` |
| `doc` | string | não | Como em funções |

Exemplo (ilustrativo):

```json
{"category":"datetime","display":"timestamp with time zone","name":"timestamptz","params":"(precision)","source":"pg_type.dat"}
```

## 6. Atualização

### 6.1 Campos do gerador e campos curados

Alguns campos são escritos pelo **gerador** a partir das fontes. Outros são **curados** por pessoas, como resumos próprios, notas e tipos do MySQL e do Databricks. Cada gerador declara quais campos são dele.

- Ao regenerar, o gerador compara **só os seus campos** e preserva os curados.
- Quando uma entrada muda e ganha uma linha nova, os campos curados são copiados para ela.
- Registros com `source: "curated"` nunca são alterados pelo gerador.

### 6.2 Ciclo semanal

1. **Detectar versões novas.**
   - PostgreSQL e MySQL: API do [endoflife.date](https://endoflife.date/api/v1/products/postgresql/), que hoje lista PostgreSQL 18.6 e MySQL 26.10, 26.7 e 9.7 [V]. Também servem as tags dos repositórios ou as imagens Docker oficiais.
   - Databricks: execução manual ou agendada com um workspace (seção 8).
2. **Atalho do PostgreSQL**: se os blobs de `kwlist.h`, `pg_proc.dat` e `pg_type.dat` na tag nova forem iguais aos da última tag processada, nada é gerado; só `verified_through` avança.
3. **Gerar o estado completo** da versão nova `v`, em memória: registros sem `since`/`until`.
4. **Resolver o estado atual** na última versão de `versions` (seção 4.4).
5. **Comparar por chave**:
   - Só no estado novo: acrescentar linha com `since: v`.
   - Só no estado atual: pôr `until: v` na linha aberta.
   - Nos dois, com campos do gerador diferentes: `until: v` na linha atual e linha nova com `since: v` (campos curados copiados).
   - Iguais: nada.
6. **Gravar**: se algo mudou, acrescentar `v` a `versions`. Em qualquer caso, `verified_through = v` e `updated_at`. Os arquivos são reescritos ordenados e canônicos, então o diff mostra só o que mudou.
7. **Publicar**:
   - Com mudança de dados, sempre um PR para revisão humana, com um resumo como "mysql 9.8.0: +12 palavras-chave, 3 funções alteradas".
   - Sem mudança de dados, só o manifesto muda. Decisão em aberto (seção 10): commit direto do robô ou PR agrupado.

### 6.3 Correções

Uma linha errada por defeito do gerador ou erro de curadoria é **corrigida no lugar**, sem `since`/`until`, e registrada no `CHANGELOG.md`. `until` significa "deixou de existir nessa versão do banco", nunca "corrigimos um erro".

## 7. Documentação e licenças

### 7.1 Regra

Texto (`summary`, `doc_file`) só pode vir de uma fonte com `redistributable: true`. Nomes, assinaturas, flags de reserva e tipos são dados factuais e podem vir de qualquer fonte. Esta regra é uma política conservadora do projeto, não um parecer jurídico. Confirme com quem puder opinar antes de tornar o repositório público.

### 7.2 Situação das fontes conhecidas

| Fonte | Texto redistribuível? | Base |
| --- | --- | --- |
| PostgreSQL: código-fonte e manual | **Sim**, mantendo os avisos de copyright e os parágrafos da licença (`LICENSE-postgresql`) | O aviso legal concede "use, copy, modify, and distribute this software and its documentation for any purpose" [V] |
| Apache Spark: código e documentação gerada | **Sim**, Apache-2.0, com NOTICE | Licença do repositório [V] |
| Manual do MySQL e tabelas de ajuda | **Não** | O manual 8.0 declara não ser GPL e proíbe publicar ou distribuir; o 8.4 e o 9.7 reservam todos os direitos não concedidos [V] |
| Documentação do Databricks | **Não**, até haver licença explícita | Nenhuma licença de reuso encontrada. O texto da Microsoft Learn costuma ser CC BY 4.0, mas o repositório dos artigos de Azure Databricks não foi encontrado [H] |
| Saída de um servidor (`INFORMATION_SCHEMA.KEYWORDS`, `sql_keywords()`, `SHOW SYSTEM FUNCTIONS`) | Só os fatos (nomes, flags); textos de uso seguem a regra da documentação do produto | `license: "facts"` |
| Conteúdo escrito pelo projeto | **Sim** | CC0-1.0 |

### 7.3 Licença do repositório

O conteúdo próprio (dados curados, resumos, esquemas, este documento) fica sob **CC0-1.0**, para maximizar o reuso. Textos extraídos de fontes permissivas mantêm a licença e os avisos delas, nos arquivos `LICENSE-*`/`NOTICE-*` do diretório do dialeto. O código do `generator/` pode ter licença própria (MIT).

## 8. Fontes de geração por dialeto

| Dialeto | Palavras-chave | Funções | Tipos |
| --- | --- | --- | --- |
| PostgreSQL | `kwlist.h` na tag `REL_{major}_{minor}`: palavra, categoria, `BARE_LABEL`/`AS_LABEL` [V] | `pg_proc.dat`: `proname`, `proargtypes`, `prorettype`, `prokind`, `proretset`, `descr` [V]. Filtrar funções internas (E/S de tipos, implementação de operadores, argumentos `internal`/`cstring`/`trigger`) [H: regras exatas] | `pg_type.dat` + mapa `typname` → grafia SQL (por exemplo `timestamptz` → `timestamp with time zone`) [H: mapa curado] |
| MySQL | Imagem Docker oficial da versão: `INFORMATION_SCHEMA.KEYWORDS` (`WORD`, `RESERVED`) [V] | Nomes e linhas de sintaxe das tabelas de ajuda (`mysql.help_topic`), sem descrições [H: formato da sintaxe] | Curados |
| Databricks | `sql_keywords()` (Databricks SQL, DBR 13.3+) com um workspace [V] | `SHOW SYSTEM FUNCTIONS` [V] + `DESCRIBE FUNCTION EXTENDED` só para assinaturas [H]; base Spark de `sql-expression-schema.md` (Apache-2.0) [V] | Curados |

A geração do Databricks exige um workspace. Ela pode rodar à mão, por exemplo mensalmente, ou agendada no CI com um token guardado como segredo, que tem custo e risco próprios.

## 9. Validação no CI

O CI do repositório rejeita um PR quando:

1. um arquivo não valida no JSON Schema do seu tipo;
2. uma linha não está em JSON canônico, ou o arquivo não está ordenado;
3. duas linhas da mesma chave têm faixas sobrepostas;
4. um `since`/`until` não existe em `manifest.versions`, ou `since ≥ until`;
5. um `source`/`summary_source` não existe em `manifest.sources`;
6. há texto de uma fonte com `redistributable: false`;
7. uma linha some sem `until` e sem entrada de correção no `CHANGELOG.md`;
8. a regeneração a partir das fontes, na última versão, não reproduz os arquivos (detecção de deriva).

## 10. Releases e consumo

### 10.1 Releases

Tags por data: `vAAAA.MM.DD`. O CI anexa à release artefatos **gerados**, que não ficam no Git:

- `snapshots/<dialeto>/<versão>.json`: um objeto JSON por versão acompanhada, com `keywords`, `functions` e `types` já resolvidos, sem `since`/`until`;
- `catalog.sqlite` opcional, com as mesmas tabelas e as faixas, para quem prefere SQL;
- `SHA256SUMS`.

`schema_version` só muda com aviso no `CHANGELOG.md`.

### 10.2 Consumir os arquivos-fonte

```text
ler <dialeto>/manifest.json
t = versão resolvida (seção 4.4)
para cada linha de keywords.jsonl, functions.jsonl e types.jsonl:
    manter se (since ausente ou since ≤ t) e (until ausente ou t < until)
```

Quem não quer lidar com faixas usa o snapshot da versão.

### 10.3 Consumo pelo Jykre Studio

- Um comando `cargo xtask catalogs sync <tag>` baixa a release fixada (ou lê um checkout local), confere o `SHA256SUMS` e copia `manifest.json` e os `.jsonl` dos três dialetos para dentro do repositório (dados vendorizados, revisados no diff).
- O app embute os arquivos com `include_str!` e os lê na primeira vez que precisa de cada dialeto.
- Em execução, resolve a versão do servidor quando o driver a conhece, ou usa a última.
- Rede só no `xtask`, sob comando, nunca no build nem em execução.

## 11. Questões em aberto

| # | Questão |
| --- | --- |
| Q1 | Nome do projeto e idioma do repositório. Inglês facilita a adoção por outras ferramentas |
| Q2 | Envolver o manifesto num [Frictionless Data Package](https://specs.frictionlessdata.io/data-package/) (`datapackage.json`) para usar validadores existentes; falta conferir como o padrão trata JSON Lines |
| Q3 | Atualizações sem mudança de dados: commit direto do robô ou PR agrupado |
| Q4 | Databricks: execução manual ou workspace no CI |
| Q5 | Regras de filtragem das funções internas do PostgreSQL [H] |
| Q6 | Formato das linhas de sintaxe nas tabelas de ajuda do MySQL [H] |
| Q7 | Se assinaturas de `DESCRIBE FUNCTION` e textos de uso do Spark podem ser associados com segurança à fonte Apache-2.0 [H] |
| Q8 | Versões iniciais acompanhadas: PostgreSQL 14+, MySQL 8.0 ou 8.4+, Databricks desde a primeira observação |
| Q9 | Se o Jykre consegue saber a versão do servidor de cada sessão, para filtrar pelo catálogo |

## Fontes

- PostgreSQL: [kwlist.h](https://github.com/postgres/postgres/blob/HEAD/src/include/parser/kwlist.h), [pg_proc.dat](https://github.com/postgres/postgres/blob/HEAD/src/include/catalog/pg_proc.dat), [pg_type.dat](https://github.com/postgres/postgres/blob/HEAD/src/include/catalog/pg_type.dat), [aviso legal](https://www.postgresql.org/docs/current/legalnotice.html), [funções de informação do sistema](https://www.postgresql.org/docs/current/functions-info.html)
- MySQL: [KEYWORDS](https://dev.mysql.com/doc/refman/8.0/en/information-schema-keywords-table.html), [ajuda no servidor](https://dev.mysql.com/doc/refman/8.4/en/server-side-help-support.html), avisos legais [8.0](https://dev.mysql.com/doc/refman/8.0/en/preface.html), [8.4](https://docs.oracle.com/cd/E17952_01/mysql-8.4-en/preface.html), [9.7](https://dev.mysql.com/doc/refman/9.7/en/preface.html)
- Databricks: [sql_keywords](https://docs.databricks.com/aws/sql/language-manual/functions/sql_keywords), [SHOW FUNCTIONS](https://docs.databricks.com/aws/en/sql/language-manual/sql-ref-syntax-aux-show-functions), [DESCRIBE FUNCTION](https://learn.microsoft.com/en-my/azure/databricks/sql/language-manual/sql-ref-syntax-aux-describe-function), [índice de funções](https://docs.databricks.com/aws/en/sql/language-manual/sql-ref-functions-builtin-alpha)
- Spark: [sql-expression-schema.md](https://github.com/apache/spark/blob/HEAD/sql/core/src/test/resources/sql-functions/sql-expression-schema.md)
- Formatos e versões: [RFC 8785 (JSON canônico)](https://www.rfc-editor.org/rfc/rfc8785), [JSON Lines](https://jsonlines.org/), [endoflife.date](https://endoflife.date/)

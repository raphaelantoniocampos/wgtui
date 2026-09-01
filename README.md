# wgtui

`wgtui` (**w**in**g**et **TUI**) é uma interface de terminal, controlada por teclado,
para o gerenciador de pacotes **winget** do Windows: procurar, instalar, atualizar e
remover pacotes, além de provisionar uma máquina a partir de um manifesto JSON
declarativo (pacotes + scripts de setup).

É a evolução do [autopkg-windows](https://github.com/raphaelantoniocampos/autopkg-windows),
agora exclusivamente winget (Chocolatey/Scoop foram descartados) e em Rust.

## Pré-requisitos

- Windows 10/11 com **winget** (App Installer). Se faltar, o wgtui pergunta se pode
  instalar via PowerShell (`Install-Module Microsoft.WinGet.Client` +
  `Repair-WinGetPackageManager`) no primeiro uso.
- Rust (edition 2024) para compilar da fonte.
- Execute como **Administrador** para instalar em `--scope machine` (o padrão). Sem
  elevação a status bar mostra `⚠ sem admin` e você deve usar `"scope": "user"` nos
  pacotes do manifesto.

## Compilar e rodar

```bash
cargo run            # desenvolvimento
cargo build --release # gera target/release/wgtui.exe
```

## Abas e teclas

| Aba | Conteúdo | Ações |
|---|---|---|
| **[1] Updates** | `winget upgrade` (lista) | `u` atualizar selecionado(s) · `U` atualizar todos · `Enter` detalhes |
| **[2] Search** | `winget search` | digite + `Enter` para buscar · `i` instalar · `Enter` detalhes |
| **[3] Installed** | `winget list` | `u` atualizar · `r` remover · `R` recarregar · `Enter` detalhes |
| **[4] Apps/Scripts** | manifesto JSON | `i` instalar/rodar selecionado(s) · `I` todos · `r`/`R` remover · `F` trocar de arquivo |

Navegação global: `Tab`/`Shift+Tab` ou `←`/`→` trocam de aba · `1`–`4` pulam para a aba ·
`/` foca o filtro · `Space` marca/desmarca · `Enter` mostra detalhes ·
`PageUp`/`PageDown` rolam o painel Output · `Esc` ou `Ctrl+C` saem.
`▶` = script · `✓` = já instalado.

Defina `WGTUI_DEBUG=1` para ver, na aba Apps/Scripts vazia, os diretórios verificados.

## Manifesto (aba Apps/Scripts)

Ao iniciar, o wgtui procura arquivos `*.json` de manifesto em (nesta ordem): a pasta do
executável, a raiz do projeto (em dev) e o diretório atual — mais as subpastas
`examples/` e `manifests/` de cada um. Um arquivo → carrega direto; vários → mostra um
seletor (`F` reabre).

### Schema canônico do wgtui

```json
{
  "packages": [
    { "id": "Google.Chrome", "name": "Google Chrome" },
    { "id": "Oracle.JavaRuntimeEnvironment", "name": "Java (x86)", "args": ["-a", "x86", "--force"] },
    { "id": "Microsoft.Office", "name": "Microsoft 365", "locale": "pt-BR" },
    { "id": "Notepad++.Notepad++", "scope": "user" }
  ],
  "scripts": [
    {
      "name": "Ativar Windows",
      "command": ["powershell", "-NoProfile", "-Command", "irm https://get.activated.win | iex"]
    }
  ]
}
```

**`packages[]`**

| Campo | Obrigatório | Descrição |
|---|---|---|
| `id` | sim | `PackageIdentifier` do winget (usado em `winget install --exact`) |
| `name` | não | Nome exibido na lista; se ausente, usa o `id` |
| `args` | não | Argumentos extras acrescentados ao `winget install` (ex.: `["-a", "x86"]`) |
| `scope` | não | Vira `--scope <valor>` (`machine` \| `user`); padrão `machine` |
| `locale` | não | Vira `--locale <valor>` (ex.: `pt-BR`) |

Comando montado: `winget install --exact <id> --silent --accept-package-agreements
--accept-source-agreements --scope <scope> [--locale <locale>] [args...]`

**`scripts[]`**

| Campo | Obrigatório | Descrição |
|---|---|---|
| `name` | sim | Nome exibido |
| `command` | sim | `argv` a executar; `command[0]` é o executável. stdout **e stderr** aparecem no painel Output |

Remover (`r`/`R`) não se aplica a scripts.

### Importação de `winget export`

Um arquivo gerado por `winget export` (`{ "Sources": [ { "Packages": [...] } ] }`) também
é lido (somente `PackageIdentifier`/`PackageName`, sem `args`/`scope`/`locale`).

Veja [`examples/packages.json`](examples/packages.json).

## Licença

MIT — veja [LICENSE](LICENSE).

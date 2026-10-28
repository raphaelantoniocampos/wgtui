# Hotkeys — referência

Mapa de todas as teclas do wgtui, por contexto. **Este arquivo é a fonte da verdade**: para
trocar uma tecla, edite aqui e o código é ajustado para bater com o doc.

- **Tecla**: a tecla que deve disparar a ação.
- **Código**: onde a tecla é tratada em `src/tui.rs` (a função; números de linha mudam).

Os contextos estão na ordem de prioridade do `handle_key`: o primeiro que estiver ativo
captura a tecla e os de baixo não a veem.

## 1. Sempre (qualquer contexto)

| Tecla | Ação | Código |
|---|---|---|
| `Ctrl+C` | Sai do app (mesmo com comando rodando) | `handle_key` (topo) |

## 2. Prompt de atualização (modal)

Captura todas as teclas enquanto aberto.

| Tecla | Ação | Código |
|---|---|---|
| `y` / `Y` | Baixa a atualização · ou sai depois de atualizar | `handle_update_key` |
| `n` / `N` / `Esc` | Esconde o prompt | `handle_update_key` |
| `Enter` | Fecha o prompt só quando a atualização falhou | `handle_update_key` |

## 3. Confirmação de saída (modal)

Aberta por `q` / `Esc` no contexto global. Captura todas as teclas enquanto aberta.

| Tecla | Ação | Código |
|---|---|---|
| `y` / `Y` / `Enter` | Sai do app | `handle_quit_confirm_key` |
| `n` / `N` / `Esc` | Cancela e volta | `handle_quit_confirm_key` |

## 4. Editor de comando `[c]`

Captura todas as teclas enquanto aberto.

| Tecla | Ação | Código |
|---|---|---|
| qualquer caractere | Insere no cursor | `handle_command_key` |
| `Backspace` | Apaga antes do cursor | `handle_command_key` |
| `Delete` | Apaga sob o cursor | `handle_command_key` |
| `←` / `→` | Move o cursor | `handle_command_key` |
| `Home` / `End` | Vai para o início / fim da linha | `handle_command_key` |
| `Enter` | Roda o comando (ou enfileira se algo estiver rodando) | `handle_command_key` |
| `Esc` | Cancela | `handle_command_key` |

## 5. Gerenciador de fila `[Q]`

Captura todas as teclas enquanto focado.

| Tecla | Ação | Código |
|---|---|---|
| `j` / `↓` | Desce na lista de pendentes | `handle_queue_key` |
| `k` / `↑` | Sobe na lista de pendentes | idem |
| `d` | Remove o pendente selecionado | idem |
| `D` | Limpa todos os pendentes | idem |
| `J` | Move o item selecionado para baixo | idem |
| `K` | Move o item selecionado para cima | idem |
| `x` | Cancela o comando em execução (mata o processo) | idem |
| `Esc` / `q` | Volta | idem |

## 6. Painel Output focado `[o]`

Captura todas as teclas enquanto focado. O painel guarda as últimas 50 execuções; ao sair,
volta para a execução atual.

| Tecla | Ação | Código |
|---|---|---|
| `j` / `↓` | Rola uma linha para baixo | `handle_output_key` |
| `k` / `↑` | Rola uma linha para cima | idem |
| `PageDown` / `PageUp` | Rola uma página | idem |
| `g` / `Home` | Vai para o início | idem |
| `G` / `End` | Vai para o fim (e volta a acompanhar a saída) | idem |
| `←` / `h` | Execução anterior no histórico | idem |
| `→` / `l` | Execução seguinte (depois da mais recente, volta à atual) | idem |
| `Esc` / `o` / `q` | Volta | idem |

## 7. Seletor de arquivo de manifesto (aba 4)

Ativo quando há vários `.json` e o seletor está aberto.

| Tecla | Ação | Código |
|---|---|---|
| `↑` / `k` | Sobe | `handle_packages_key` (bloco `packages_file_picker`) |
| `↓` / `j` | Desce | idem |
| `Enter` | Carrega o arquivo | idem |
| `Esc` | Fecha o seletor | idem |

## 8. Filtro focado (`/`)

| Tecla | Ação | Código |
|---|---|---|
| qualquer caractere | Digita no filtro (inclui `j`/`k`/`q`...) | `handle_key` (bloco `filter_focused`) |
| `Backspace` | Apaga o último caractere | idem |
| `Enter` | Aba Search: roda `winget search` · demais abas: sai do filtro | idem |
| `Esc` | Sai do filtro | idem |
| `↑` / `↓` | Sai do filtro e move na lista | idem |
| `Tab` / `Shift+Tab` / `←` / `→` | Trocam de aba (passam direto) | idem |

## 9. Global (lista, fora dos modos acima)

| Tecla | Ação | Código |
|---|---|---|
| `↑` / `k` | Sobe na lista (no topo, foca o filtro) | `handle_key` (match principal) |
| `↓` / `j` | Desce na lista | idem |
| `1` `2` `3` `4` | Pula para a aba | idem |
| `←` / `Shift+Tab` / `h` | Aba anterior | idem |
| `→` / `Tab` / `l` | Próxima aba | idem |
| `/` | Foca o filtro | idem |
| `Space` | Marca / desmarca o item | idem |
| `a` | Marca / desmarca todos | idem |
| `Enter` | Mostra detalhes | idem |
| `c` | Abre o editor de comando com o último comando | idem |
| `Q` | Abre o gerenciador de fila (só se houver algo rodando/pendente) | idem |
| `o` | Foca o painel Output (rolagem e histórico de execuções) | idem |
| `PageUp` / `PageDown` | Rola o painel Output uma página | idem |
| `q` / `Esc` | Pede confirmação para sair do app | idem |

## 10. Por aba

Só chegam aqui teclas que o global não tratou.

### [1] Updates

| Tecla | Ação | Código |
|---|---|---|
| `u` | Atualiza os selecionados | `handle_updates_key` |
| `U` | Atualiza todos | idem |
| `w` | Instala atualizações do Windows | idem |
| `r` | Verifica as atualizações de novo (`winget upgrade`) | idem |

### [2] Search

| Tecla | Ação | Código |
|---|---|---|
| `i` | Instala os selecionados | `handle_search_key` |

### [3] Installed

| Tecla | Ação | Código |
|---|---|---|
| `u` / `U` | Atualiza os selecionados | `handle_installed_key` |
| `x` | Remove os selecionados | idem |
| `r` | Recarrega a lista | idem |

### [4] Apps/Scripts

| Tecla | Ação | Código |
|---|---|---|
| `i` | Instala/roda os selecionados | `handle_packages_key` |
| `I` | Instala/roda todos | idem |
| `x` | Remove os selecionados | idem |
| `X` | Remove todos | idem |
| `f` / `F` | Reabre o seletor de arquivo (se houver mais de um) | idem |

## Ao trocar uma tecla, atualize também

Além do handler, o texto da tecla aparece em vários lugares:

1. **Barra de status**: `Tab::hints()` (por aba) e `render_status_bar` (globais, fila e
   painel Output focado).
2. **Títulos dos painéis**: dica da fila (`" j/k move · d remove ..."`), do editor
   `[c]` (`" ←→ move · del · enter run · esc cancel "`) e do Output (`" o: scroll · history "`,
   e focado `" j/k scroll · ←/→ history · g/G top/bottom · esc back "`); prompts modais de
   atualização (`[y]`/`[n]`/`[esc]`) e de saída (`[y/enter]`/`[n/esc]`).
3. **Testes** em `src/tui.rs` (`mod tests`) que pressionam a tecla antiga.
4. **README.md**, seção "Abas e teclas".
5. **Este arquivo.**

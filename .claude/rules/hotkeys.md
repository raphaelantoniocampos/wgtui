# Hotkeys: docs/HOTKEYS.md é a fonte da verdade

`docs/HOTKEYS.md` define as teclas do wgtui. O usuário edita esse arquivo à mão; o código
segue o documento, nunca o contrário.

## No início de qualquer tarefa que toque teclas, e sempre que o usuário pedir

1. Leia `docs/HOTKEYS.md` e compare cada linha (coluna **Tecla**) com os handlers em
   `src/tui.rs` (as funções da coluna **Código**).
2. Se o doc divergir do código (tecla, ação ou contexto), atualize o código para bater com
   o doc. Teclas que o código trata mas o doc não lista devem ser removidas do código.
3. Antes de mudar, avise o usuário de conflitos: a mesma tecla usada duas vezes no mesmo
   contexto, ou uma tecla que um contexto de prioridade mais alta já captura (a ordem das
   seções do doc é a ordem de prioridade do `handle_key`). Não resolva conflito por conta
   própria — pergunte.

## Como aplicar uma troca

- Trabalhe test-first: ajuste/crie o teste em `src/tui.rs` (`mod tests`) com a tecla nova,
  veja falhar, depois mude o handler.
- Atualize todos os lugares listados em "Ao trocar uma tecla, atualize também" no doc:
  `Tab::hints()`, `render_status_bar`, dicas nos títulos dos painéis, testes, README.
- Rode `cargo fmt`, `cargo clippy -- -D warnings` e `cargo test`.

## Quando o código mudar por outro motivo

Se você adicionar, remover ou mudar qualquer tecla em `src/tui.rs` por outra tarefa,
atualize `docs/HOTKEYS.md` no mesmo commit para o doc continuar refletindo o código.

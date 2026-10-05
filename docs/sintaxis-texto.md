# Sintaxis de texto de StardustLang (propuesta)

Estado: **implementado** en [`src/lang/`](../src/lang/) (formato principal). Ejemplos en
[`programs/texto/`](../programs/texto/), con tests de equivalencia contra los `.json` en
[`tests/texto.rs`](../tests/texto.rs).

## Por qué

StardustLang se escribe hoy como JSON. Para la VM es perfecto, pero es mal formato para que lo
genere un LLM, sobre todo uno pequeño en el teléfono:

- **Gasta muchos tokens.** Los 10 programas de ejemplo ocupan 4.144 tokens en JSON compacto y
  1.198 en esta sintaxis (un 71 % menos; la calculadora, un 80 % menos).
- **Es una notación que el modelo nunca ha visto.** `r = number(m) * 2` es Python; su equivalente
  JSON es `{"op":"MATH","target":"r","operator":"*","left":{"parse":{"var":"m"}},"right":2}`, más
  una variable temporal con `COMPARE` por cada condición.

Un formato binario no ayuda: el modelo solo emite texto, y el binario en base64 cuesta el doble de
tokens que el JSON y es imposible de escribir correctamente.

## Principios

1. **El JSON sigue siendo la representación interna (IR).** Un compilador en Rust traduce
   `.stardust` → `Program`. La VM, `check.rs`, la web y los programas existentes no cambian.
2. **Parecerse a Python.** Indentación, `if/elif/else`, `while`, `for x in xs`, `def`, `return`,
   f-strings, `xs.append(x)`, `', '.join(xs)`. Lo que el modelo escribiría por instinto, funciona.
3. **Todo baja a las instrucciones que ya existen.** Las expresiones con operadores se compilan a
   `MATH`/`COMPARE` sobre variables temporales; los bucles recalculan su condición solos.
4. **Menos cosas que declarar.** Las capacidades se deducen del uso; `entry` es el primer actor.

## Un vistazo

```python
app doble

actor UI:
  start:
    r = 'Escribe un número'
  on m:
    r = number(m) * 2
  view:
    column:
      label(r)
      input('Doble')
      button('Cero', send='0')
```

Compila al mismo programa que el ejemplo del `SKILL.md` actual: 59 tokens frente a 130.

## Estructura de un programa

```python
app nombre                         # obligatorio: "program" de la IR
allow 'https://api.github.com/'    # net_allow (repetible)
export Areas                       # exports (repetible)
grant 'cap-geo-123': Areas         # grants: token → actores
entry IO                           # opcional: por defecto, el primer actor

actor Nombre:                      # un actor
  uses FILE, NET                   # opcional: ver «Capacidades»
  start:                           # on_start
    ...
  on m:                            # on_message, el mensaje queda en m
    ...
  def f(a, b):                     # procedures (scope aislado, recursión)
    ...
    return expr
  view:                            # view (el actor recibe "RENDER")
    ...
```

Variantes de `on`:

| Sintaxis | IR |
|---|---|
| `on m:` | `"on_message":{"bind":"m", …}` |
| `on p from cliente:` | `… "reply_to":"cliente"` (dirección de quien llamó) |
| `on p(base: int, altura: int):` | `… "expects":{"base":"int","altura":"int"}` |

**Capacidades.** Se deducen de lo que usa cada actor: `print`/`input` → `IO_STREAM`, `view` →
`RENDER`, ficheros → `FILE`, `sign`/`verify` → `CRYPTO`, `fetch`/`sock_send`/`send('actor://…')`
→ `NET`. El JSON resultante las lista explícitas, así que siguen siendo auditables. Con `uses` se
declaran a mano y entonces el compilador **falla** si el actor usa algo no declarado.

## Sentencias

| Sintaxis | IR |
|---|---|
| `x = expr` | `DEF_VAR` (primera vez en el scope) o `ASSIGN` |
| `a, b = b, a + b` | evalúa todo a temporales y luego asigna (asignación simultánea) |
| `x += 1` (y `-= *= /=`) | `MATH` |
| `if c:` / `elif c:` / `else:` | `IF_COND` anidados |
| `while c:` | `LOOP`; `c` se calcula antes del bucle y al final del cuerpo |
| `for x in xs:` / `for i, x in enumerate(xs):` | `FOREACH` (con `index`) |
| `xs.append(v)` | `APPEND` |
| `send(Actor, v)` · `send('app/Actor', v, cap='tok')` · `send(cliente, v)` | `SEND` |
| `print(a, b, …)` | `IO_STREAM out` (`print('txt: ', v)` → `prompt` + `value`) |
| `x = input('¿n? ')` | `IO_STREAM in` |
| `x = f(a)` (con `def f` en el actor) | `CALL … into` |
| `return expr` | asigna el valor de retorno (ver «Bajada a la IR») |
| `write(p, v)` · `write(p, v, append=True)` · `delete(p)` | `FILE_WRITE` · `FILE_APPEND` · `FILE_DELETE` |
| `fetch(url, method='POST', headers={…}, body=v, tag=t)` | `NET_FETCH` (la respuesta llega a `on`) |
| `sock_send('sock://h:p', v, tag=t)` | `SOCK_SEND` |
| `pass` | nada |

En `send`, un nombre que coincide con un actor de la app es ese actor; si no, es una variable (p.
ej. la de `from`). Si una variable se llama igual que un actor, es un error de compilación.

## Expresiones

| Sintaxis | IR |
|---|---|
| `42` `3.5` `'texto'` `"texto"` `True`/`true` `False`/`false` | literales |
| `x` | `{"var":"x"}` |
| `[a, b]` · `{nombre: n, 'user-agent': 'x'}` | `list` · `record` |
| `m.campo` · `m['campo']` · `xs[0]` · `xs[-1]` | `field` · `field` · `at` · `at` |
| `a + b` `a - b` `a * b` `a / b` `a % b` | `MATH` a una temporal |
| `a == b` `!=` `<` `>` `<=` `>=` | `COMPARE` a una temporal |
| `a and b` · `a or b` · `not a` | `IF_COND` (con cortocircuito) |
| `x in [a, b]` · `'sub' in texto` | `x == a or x == b` · `contains` |
| `f'Total: {n} €'` | concatenación con `+` |
| `$input` `$name` `$bytes` | marcadores de host (solo en `send`/`submit`/`drop` de la vista) |

Precedencia, de menor a mayor: `or`, `and`, `not`, comparaciones e `in`, `+ -`, `* / %`, menos
unario, acceso (`.campo`, `[i]`, llamadas).

**Diferencia con Python:** `/` entre dos enteros es división entera (`7 / 2` → `3`), como en la
VM. Con algún decimal es real (`7 / 2.0` → `3.5`). Se acepta `//` como sinónimo.

### Funciones integradas

Puras (se traducen a una expresión de la IR):

| Python / función | IR |
|---|---|
| `len(x)` | `len` |
| `str(x)` | `to_str` |
| `number(x)` · `int(x)` · `float(x)` | `parse` (auto-tipa `int` > `float` > `date` > texto) |
| `date('2026-12-31')` | `{"lit":…,"as":"date"}` |
| `t.upper()` `t.lower()` `t.strip()` | `upper` `lower` `trim` |
| `t.split(',')` · `', '.join(xs)` | `split` · `join` |
| `t[1:4]` · `t.replace(a, b)` · `t.find(s)` | `slice` · `replace` · `index_of` |
| `t.startswith(p)` · `t.endswith(s)` · `repeat(t, n)` | `starts_with` · `ends_with` · `repeat` |
| `bytes(t)` · `text(b)` · `base64(b)` | `to_bytes` · `from_bytes` · `base64` |

Respaldadas por una instrucción (el compilador las saca a una temporal; pueden ir dentro de
cualquier expresión): `read(p)`, `read(p, bytes=True)`, `exists(p)`, `files()`, `files('dir/')`,
`hash(v)`, `sign(v)`, `verify(v, firma)`, `serialize(v)`, `deserialize(t)` → `FILE_READ`,
`FILE_EXISTS`, `FILE_LIST`, `HASH`, `SIGN`, `VERIFY`, `SERIALIZE`, `DESERIALIZE`.

## Vista

Los contenedores llevan `:` y sus hijos indentados; las hojas son llamadas:

| Sintaxis | Widget |
|---|---|
| `column:` · `row:` · `grid(4):` | `column` · `row` · `grid` |
| `label(expr)` | `label` |
| `button(etiqueta, send=v, to=Actor)` | `button` (sin `to`: al propio actor) |
| `input(etiqueta, placeholder='…', submit=$input, to=Actor)` | `input` (`submit` por defecto: `$input`) |
| `textarea(etiqueta, src=v, placeholder='…', to=Actor)` | `textarea` |
| `image(src, alt=t)` · `html(src)` | `image` · `html` |
| `filedrop(etiqueta, drop=…, to=Actor)` | `filedrop` (`drop` por defecto: `{name: $name, bytes: $bytes}`) |
| `for x in xs:` (+ `else:` opcional) | `list` (`else` = `empty`, se muestra si está vacía) |

Dos comodidades que la IR no tiene y el compilador resuelve:

- **`for` sobre una lista literal se desenrolla** al compilar: `for k in [7, 8, 9]: button(k, send=k)`
  genera tres botones. Así las 16 teclas de la calculadora son 2 líneas.
- **Expresiones calculadas en la vista.** La IR solo admite expresiones puras en `view` (no puede
  hacer `MATH`). Si la vista usa algo como `label(f'Tareas: {len(tareas)}')`, el compilador crea
  una variable derivada (`_v1`) y añade su cálculo al final de `start` y de `on`. Es el error más
  común hoy: el modelo escribe la operación en la vista y la VM no puede evaluarla.

## Bajada a la IR

Ejemplo, de `procedures.stardust`:

```python
def fib(k):
  if k < 2:
    return k
  else:
    return fib(k - 1) + fib(k - 2)
```

```jsonc
"fib": { "params": ["k"], "returns": "_r", "body": [
  {"op":"COMPARE","target":"_1","operator":"<","left":{"var":"k"},"right":2},
  {"op":"IF_COND","cond":{"var":"_1"},
   "then":[{"op":"ASSIGN","target":"_r","value":{"var":"k"}}],
   "else":[
     {"op":"MATH","target":"_2","operator":"-","left":{"var":"k"},"right":1},
     {"op":"CALL","proc":"fib","args":[{"var":"_2"}],"into":"_3"},
     {"op":"MATH","target":"_4","operator":"-","left":{"var":"k"},"right":2},
     {"op":"CALL","proc":"fib","args":[{"var":"_4"}],"into":"_5"},
     {"op":"MATH","target":"_r","operator":"+","left":{"var":"_3"},"right":{"var":"_5"}}]}]}
```

Reglas:

- **Temporales** `_1`, `_2`, … (el prefijo `_` queda reservado; los visores de estado pueden
  ocultarlas). Las expresiones puras van directas a la IR, sin temporal.
- **`while c`**: `c` se calcula en una temporal antes del `LOOP` y otra vez al final del cuerpo.
  Es el «patrón obligatorio» de hoy, ahora automático.
- **`and`/`or`** se bajan a `IF_COND` con cortocircuito, porque la IR no tiene operadores lógicos.
- **`return`** solo puede ir en posición final (última sentencia del `def`, o última de cada rama
  de un `if` final), porque la IR no tiene salida anticipada. Si no, error de compilación.

## Errores

El compilador informa por **línea y columna**, con el mismo estilo que `--check` (mensaje + forma
correcta):

```
línea 6, col 9: 'parse' no existe; para convertir texto a número usa number(m)
línea 12: 'return' solo puede ir al final de la función (la IR no tiene salida anticipada)
línea 4: el actor 'Logica' no existe (¿quisiste decir 'Logic'?)
```

Los errores semánticos que ya detecta `check.rs` (variable nunca definida, actor inexistente…) se
siguen calculando sobre la IR y se traducen a líneas con un mapa de origen (cada instrucción
generada recuerda la línea de la que salió).

## Qué no hay (respecto a Python)

Sin `break`/`continue`, sin `return` anticipado, sin clases, `import`, `lambda`, funciones anidadas
ni excepciones. `/` entre enteros es entera. Las funciones solo existen dentro de su actor.

## Cambio en la VM

Uno solo (ya aplicado), y es una corrección: hoy `contains` convierte las listas a texto, así que `1 in [10]` daría
verdadero. Debería comprobar si es miembro de la lista. Con eso, `x in xs` funciona también cuando
`xs` es una variable y no solo con listas literales.

## Decisiones tomadas

1. **Nombre y extensión:** `.stardust` para la sintaxis de texto; el JSON es la IR.
2. **Bloques por indentación**, como Python.
3. **`/` entre enteros es entera**, como en la VM.
4. **Capacidades deducidas del uso**, con `uses` como declaración explícita opcional.
5. **El texto es el formato principal**; el JSON sigue documentado como IR.

## Plan de implementación

1. Lexer con indentación + parser de expresiones (precedencias) y sentencias → AST.
2. Bajada AST → `Program` (temporales, `while`, `and/or`, `return`, vista derivada).
3. Mapa de origen y errores por línea; `check.rs` sobre la IR traducido a líneas.
4. CLI: `stardust app.stardust` (ejecuta), `--check`, y `--emit-json` para ver la IR.
5. WASM: `compile_program(src)`; la skill y el playground aceptan `.stardust`.
6. Test: cada `programs/texto/*.stardust` compila y su ejecución coincide con la del `.json` original.

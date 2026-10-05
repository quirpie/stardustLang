# Bytecode binario de StardustLang (propuesta)

Estado: **aplazado**. Propuesta para la decisión abierta «¿bytecode binario?» del README; por ahora
la VM sigue ejecutando la IR, suficiente para los fines demostrativos del proyecto.

## Qué cambia y qué no

```text
hoy:        .stardust ──compilador──▶ IR (JSON) ──────────────▶ VM (intérprete de árbol)
propuesta:  .stardust ──compilador──▶ IR (JSON) ──ensamblador──▶ .akb (bytecode) ──▶ VM (intérprete de bytecode)
```

- **Se escribe en `.stardust`**, igual que ahora (personas y LLMs). El bytecode no se escribe a mano.
- **Se ejecuta y se distribuye `.akb`**: es el formato que la VM carga, el que viaja entre nodos
  y el que se instala en el teléfono o el navegador.
- **La IR en JSON queda como paso interno** del compilador y como vista de depuración
  (`--emit-json`). Los `programs/*.json` existentes siguen funcionando: se ensamblan al cargarlos.

## Qué se gana

1. **Velocidad.** Las variables se resuelven al compilar a posiciones numéricas («slots») en lugar
   de buscarse por nombre en un `HashMap` en cada acceso, y el control de flujo son saltos en vez de
   recorrer el árbol. Al arrancar no hay que parsear JSON.
2. **Tamaño.** Nombres y textos se guardan una sola vez en una tabla; los números, en varints.
3. **Integridad y firma.** Un módulo `.akb` puede llevar una firma Ed25519 (la misma identidad que
   ya usa `actor://`), y la VM puede negarse a ejecutar módulos sin firmar o de autores no
   autorizados. Es importante para distribuir apps al teléfono o entre nodos.
4. **Medición y límites exactos.** Contar instrucciones ejecutadas sustituye al tope de vueltas de
   `LOOP` (que hoy no protege, p. ej., de una recursión ancha).
5. **Errores con línea.** El módulo lleva un mapa `pc → línea`, así que un fallo en ejecución dice
   «FALLO en línea 12» del `.stardust`, no solo el mensaje.

## Qué cuesta

- **Reescribir el intérprete** (`src/interpreter.rs`, ~1.150 líneas) y adaptar `vm.rs`, `wasm.rs` y
  `render.rs` a memoria por slots. Es el núcleo de la VM: el riesgo está en cambiar sutilmente la
  semántica. Se mitiga con **pruebas diferenciales**: los dos intérpretes conviven hasta que
  todos los programas producen exactamente la misma traza y salida en ambos.
- **Depurar es menos directo.** Se compensa con un desensamblador (`--disasm`) y el mapa de líneas.
- **Un formato más que mantener**, con versión: un `.akb` antiguo debe rechazarse con un mensaje
  claro, no fallar de forma extraña.

## Formato del módulo `.akb`

Todos los enteros son varints LEB128 salvo donde se indica.

```text
cabecera   "AKB" 0x00 · versión (u16) · flags (u16: firmado, con mapa de líneas)
strings    n · [longitud · bytes UTF-8]          ← nombres, textos, claves de record
consts     n · [tipo (u8) · valor]               ← int, float, str(idx), bool, date, bytes
programa   nombre(str) · entry(actor) · net_allow[str] · exports[actor] · grants[(str, [actor])]
actores    n · [nombre(str) · capacidades (u8, bitmask) · slots[str] · on_start · on_message
                · procedures[(nombre, nº params, nº locales, código)] · view]
código     instrucciones (ver abajo); cada bloque termina en RET
líneas     opcional: [(pc, línea)]
firma      opcional: clave pública (32 B) · firma Ed25519 (64 B) de todo lo anterior
```

`on_message` guarda el slot donde se enlaza el mensaje (`bind`), el de `reply_to` y el contrato
`expects`. La vista (`view`) sigue siendo un árbol declarativo; sus expresiones, que el compilador
ya deja puras, se codifican como árboles pequeños que leen slots y constantes.

## Juego de instrucciones

Máquina de registros: los operandos son **slots** de la memoria del actor (o locales del
procedimiento) o **constantes**; un bit de etiqueta en el operando dice cuál.

| Grupo | Instrucciones |
|---|---|
| Datos | `MOV d a` · `NEWLIST d n a…` · `RECORD d n (k a)…` · `APPEND s a` |
| Aritmética | `ADD SUB MUL DIV MOD d a b` (misma promoción de tipos que hoy) |
| Comparación | `EQ NE LT GT LE GE d a b` |
| Control | `JMP off` · `JMPF a off` · `JMPT a off` · `ITER i lista` · `NEXT i x idx off` · `CALL p n a… d` · `RET` |
| Acceso | `FIELD d a str` · `AT d lista i` · `LEN d a` |
| Texto y bytes | `SPLIT JOIN SLICE REPLACE CONTAINS STARTS ENDS INDEXOF UPPER LOWER TRIM REPEAT TOSTR PARSE TOBYTES FROMBYTES B64` |
| Efectos | `SEND to a [cap]` · `OUT` · `IN` · `FWRITE FAPPEND FREAD FEXISTS FLIST FDELETE` · `HASH SIGN VERIFY` · `SER DESER` · `FETCH` · `SOCK` |

Cada instrucción con efecto comprueba la capacidad del actor igual que hoy. Ejemplo (el `while` de
`fibonacci.stardust`):

```text
 0  LEN     s5 s3            ; _1 = len(serie)
 1  LT      s6 s5 s0         ; _2 = _1 < limit
 2  JMPF    s6 +6
 3  APPEND  s3 s1            ; serie.append(a)
 4  ADD     s7 s1 s2         ; _3 = a + b
 5  MOV     s1 s2            ; a = b
 6  MOV     s2 s7            ; b = _3
 7  JMP     -7
 8  JOIN    s8 s3 k0         ; ', '.join(serie)
```

## Memoria por slots

La memoria de un actor pasa de `HashMap<nombre, valor>` a un `Vec<valor>` indexado por slot, con la
tabla de nombres del módulo al lado. Así la vista de estado, `snapshot()`/`restore()` y la web
siguen viendo nombres. Una variable aún sin asignar es «vacía» y leerla da el mismo error que hoy
(«variable no definida»).

## Plan por fases

1. **Formato y ensamblador.** IR → `.akb`, desensamblador (`stardust --disasm`), `stardust --build`
   para generar `.akb`, test de ida y vuelta. La VM aún no cambia.
2. **Intérprete de bytecode en paralelo.** Se elige con `--engine bc`. Pruebas diferenciales: cada
   programa de `programs/` (JSON y `.stardust`) da la misma salida y traza en ambos intérpretes.
3. **El bytecode pasa a ser lo que se ejecuta por defecto,** en el CLI, en `--serve` y en el
   `Engine` de WASM; fallos con línea de origen.
4. **Retirar el intérprete de árbol.** Firma de módulos y política «solo módulos firmados» (opcional
   por nodo).

## Decisiones para ti

1. **Extensión `.akb`** para los módulos.
2. **¿Firmar módulos?** Propuesta: sí, opcional al compilar, y que cada nodo decida si exige firma.
3. **`programs/*.json`:** seguir aceptándolos (se ensamblan al cargarlos) o migrar todos los
   ejemplos a `.stardust` y dejar el JSON solo como depuración.
4. **Orden:** propuesta, fases 1 y 2 juntas (formato + intérprete en paralelo con pruebas
   diferenciales), y no pasar a la 3 hasta que todo coincida.

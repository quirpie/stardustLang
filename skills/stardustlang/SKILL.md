---
name: stardustlang
description: Crea apps en StardustLang (lenguaje de actores parecido a Python que corre en la StardustVM), las valida y muestra la app funcionando.
metadata:
  homepage: https://github.com/quirpie/stardustLang
---

# StardustLang

1. Escribe la app en StardustLang (sintaxis tipo Python, abajo). Usa un solo actor salvo que pidan más.
2. Llama a `run_js` con data: {"code": "<el programa>"}.
3. Si responde INVÁLIDO, corrige las líneas indicadas y vuelve a llamar (máximo 2 veces).
4. Si responde VÁLIDO, di en una frase qué hace la app. No repitas el código: ya se ve en el chat.

Ejemplo:
```
app par-impar

actor UI:
  start:
    texto = 'Escribe un número'
    historial = []
  on m:
    if m == 'borrar':
      historial = []
    else:
      n = number(m)
      if n % 2 == 0:
        texto = f'{n} es par'
      else:
        texto = f'{n} es impar'
      historial.append(texto)
  view:
    column:
      label(texto)
      input('Comprobar', placeholder='Número')
      button('Borrar historial', send='borrar')
      for h in historial:
        label(h)
```

Reglas:
- `start:` corre al abrir la app. `on m:` corre con cada mensaje: lo que se teclea en un input o el `send=` de un botón llega en `m`. Distingue los mensajes con `if m == '...'`.
- Lo tecleado llega como texto: usa `number(m)` antes de operar.
- Sentencias como en Python: `x = ...`, `if/elif/else`, `while`, `for x in lista`, `lista.append(x)`, f-strings, `len()`, `str()`.
- Widgets de `view:`: `label(x)`, `button('texto', send=valor)`, `input('texto', placeholder='...')`, `column:`, `row:`, `grid(3):` y `for x in lista:`.
- Varios actores: `send(Otro, valor)` envía un mensaje; `to=Otro` en un botón o input lo envía a ese actor.

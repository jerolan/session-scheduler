---
name: verify-session-scheduler
description: Verifica Session Scheduler en Windows y el flujo de MCP Events hacia Dot. Úsala para revisar disponibilidad, ejecutar las pruebas existentes o comprobar la suscripción y la entrega de turnos autorizados.
---

# Verificar Session Scheduler

## Disponibilidad de solo lectura

- Desde el proxy de Windows, lee `AGENTS.md`, comprueba el acceso por
  `codex-linux` y revisa el estado del worktree indicado. Ejecuta Git y las
  pruebas de Linux en el repositorio remoto.
- En Windows, comprueba que el scheduler existente escucha en el puerto de
  loopback esperado y que su estado local tiene una ACL protegida para el
  propietario. Comprueba el registro local de Codex Desktop, la configuración
  del helper de headers y la aprobación de herramientas. Usa el cliente
  configurado para `initialize`, `tools/list` y `events/list` según la
  interfaz disponible. En Desktop confirma `list_sessions`, incluso si la
  lista está vacía. En el catálogo comprueba `turn.completed`.
- Para la MCP App, comprueba por separado que la app existe en el workspace de
  ChatGPT y está habilitada para el Dot de la cuenta conectada. Confirma
  descubrimiento a través del transporte elegido. Con Secure MCP Tunnel,
  comprueba estado del cliente y asociación y permisos del túnel. Para
  HTTPS directo, comprueba alcance, confianza TLS y autenticación desde el
  lado de ChatGPT. [Rutas de conexión](../../../README.md#registro-de-la-mcp-app-en-chatgpt-y-dot)
- Distingue el bearer del scheduler, la credencial de ejecución del plano de
  control del túnel, los permisos de Platform y ChatGPT, la aprobación de
  herramientas y el secreto de firma de cada suscripción. Usa los
  mecanismos de credenciales existentes. La revisión de disponibilidad
  termina aquí si solo se autorizó lectura.

## Pruebas existentes

En Linux, usa la toolchain existente. Las pruebas Python requieren el binario
de depuración y usan fixtures ACP. Ejecuta las pruebas en un entorno aislado de
la configuración activa, en particular de `SESSION_SCHEDULER_*`.

```sh
cargo test --locked
cargo build --locked
python3 tests/acceptance.py -v
python3 tests/http_events.py -v
```

El smoke test de PowerShell 7 requiere el ejecutable de Windows compilado para
la revisión bajo prueba y `tests/acp_fixture_windows.rs` compilado como
ejecutable independiente con el mismo target. Usa las herramientas de
compilación disponibles y una carpeta temporal para los artefactos de prueba.
Ejecuta el script existente en Windows, en un proceso separado del runtime,
sin variables de la instalación activa. El script usa loopback y estado
desechable. [Smoke test de Windows](../../../tests/windows_private_events_smoke.ps1)

```powershell
pwsh -NoProfile -File tests/windows_private_events_smoke.ps1 -SchedulerExe <scheduler.exe> -FixtureExe <fixture.exe>
```

Registra el resultado de cada suite. Los fixtures comprueban el protocolo, la
firma, la correlación y, en Windows, la persistencia de la suscripción y la
ACL. La suscripción real de ChatGPT y su reacción se verifican en los pasos
siguientes.

## Suscripción y challenge

- Usa una suscripción de `turn.completed` de la cuenta conectada en la app.
  Si ya existe, reúne su evidencia sin resuscribirla. Crea una suscripción de
  prueba solo con autorización explícita y mediante el flujo nativo de la
  app. Registra si existía antes de la verificación. Una llamada
  `events/subscribe` para la misma cuenta, URL, evento y argumentos puede
  renovar o actualizar una suscripción existente.
- Registra por separado la confirmación de la suscripción, su ID y
  `refreshBefore`, y la evidencia disponible del challenge. El scheduler
  envía un webhook firmado al callback HTTPS y requiere una respuesta
  satisfactoria que devuelva el challenge. La verificación puede estar en
  caché. Documenta si viste un challenge nuevo, si hay evidencia previa o
  si la verificación quedó sin observar. [Challenge de MCP Events](https://developers.openai.com/plugins/build/mcp-events#verify-the-callback)

## Entrega de extremo a extremo

Solo con autorización explícita para abrir un turno real, pide a Dot que
delegue en el workspace acordado la respuesta `pong`. Conserva los IDs de
sesión y turno devueltos por la delegación. Busca el `turn.completed` firmado
con los mismos IDs, el ID de la suscripción, `eventId` y el motivo de cierre
`end_turn`. Registra la recepción por la plataforma y si Dot comunica
`pong` sin una segunda solicitud. Para el caso de fallo de
[#3](https://github.com/jerolan/session-scheduler/issues/3), usa un turno de
fallo confirmado autorizado y verifica de forma independiente el motivo
`failed` con la misma correlación. Si falta evidencia, deja pendiente ese
criterio. Usa turnos reales autorizados para esta evidencia y limita la
emisión sintética a los fixtures aislados.

## Limpieza y diagnóstico

- Con autorización de limpieza, elimina solo la suscripción de prueba
  creada para esta verificación. Comprueba la cuenta, el ID y el alcance
  antes de usar el flujo de la app. Si se desconoce el propietario, deja la
  suscripción intacta. No edites el almacén de suscripciones a mano.
- Reporta el punto exacto del fallo y la evidencia mínima. Distingue
  conectividad, auth y descubrimiento, registro de la suscripción,
  validación saliente de DNS o TLS y challenge, entrega del evento y
  reacción de Dot. Registra códigos HTTP o JSON-RPC, categorías de error,
  horas y referencias de correlación. En informes compartidos usa alias
  consistentes para identificadores privados.
- Excluye valores de credenciales, headers crudos, firmas, challenges,
  URLs privadas completas, estado serializado y volcados del entorno. Usa
  resultados de presencia o validación y categorías de error. Conserva el
  runtime y las credenciales existentes durante la verificación.

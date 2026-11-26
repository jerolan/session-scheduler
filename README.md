# Session Scheduler

Session Scheduler delega turnos a Codex CLI y OpenCode mediante ACP y ofrece el evento
`turn.completed` para avisar a Dot cuando el turno tiene un desenlace confirmado.

## Confianza del workspace por MCP

En el MCP HTTP autenticado, `delegate` recibe una ruta absoluta a una carpeta
existente. Si todavía no es de confianza, devuelve `approval_required`, la
ruta efectiva y un `attempt_id`, sin iniciar una sesión. El solicitante muestra
esa ruta y pide un único «sí» explícito en el chat. Tras recibirlo, llama a
`approve_workspace` con el `attempt_id`; la herramienta guarda la confianza y
continúa el encargo original. Ante una negativa o ausencia de respuesta no se
llama a la herramienta. Una ruta inválida o inaccesible devuelve
`invalid_workspace`.

La confianza se guarda en el directorio privado de estado y se aplica solo a
la carpeta efectiva exacta. En HTTP, las rutas de
`SESSION_SCHEDULER_WORKSPACES` se consideran de confianza por defecto.

Para retirar la confianza, pide en el chat que se revoque la carpeta. El
solicitante llama a `revoke_workspace` con la ruta absoluta en `workspace`.
El resultado `revoked` confirma la ruta efectiva. La revocación surte efecto
sin reiniciar, se conserva tras reiniciar y prevalece incluso si esa
carpeta sigue en `SESSION_SCHEDULER_WORKSPACES`. Las otras carpetas conservan
sus permisos.

El turno activo puede terminar y el historial disponible se puede consultar
con `inspect_session`. Una nueva delegación devuelve `approval_required`.
`follow_up` y `reconnect_session` devuelven `workspace_not_trusted` cuando
el workspace de la sesión ya no es de confianza.

Para volver a autorizar la carpeta, usa el flujo de aprobación de una nueva
delegación. Tras el «sí» explícito, `approve_workspace` registra la confianza,
continúa ese encargo y vuelve a habilitar el seguimiento y la reconexión de
las sesiones que sigan disponibles. La revocación invalida los intentos
pendientes anteriores de esa carpeta. Repetir la aprobación de un intento
que ya se inició devuelve los IDs originales sin ejecutar otro turno ni
restaurar la confianza. Los intentos solo viven durante el proceso.

La aprobación y la revocación se aplican al Scheduler que sirve el MCP HTTP
autenticado. Dot se conecta exclusivamente a ese endpoint `/mcp` a través
de la MCP App de ChatGPT. Un proceso iniciado por separado con stdio usa su
propia lista de workspaces al arrancar, al margen de las decisiones de
confianza de HTTP.

## Selección de agente por MCP

La herramienta `delegate` recibe `workspace`, `prompt` y el campo opcional
`agent`. Usa `"agent": "opencode"` para elegir OpenCode. Al enviar
`"agent": "codex"` u omitir el campo, el scheduler usa Codex CLI.
`inspect_session` incluye el agente elegido y el ID de sesión ACP cuando se
ha abierto.

Configura `SESSION_SCHEDULER_OPENCODE_COMMAND` en el proceso del scheduler
con la ruta al ejecutable de OpenCode. El scheduler le pasa el argumento
`acp` y abre la sesión en el workspace exacto autorizado por configuración o por el MCP
HTTP autenticado. OpenCode usa la política de permisos
del entorno que lo ejecuta. El scheduler cancela las solicitudes ACP de
permiso interactivo de ambos agentes. Codex sigue usando
`SESSION_SCHEDULER_ACP_COMMAND`.

## Importar una sesión nativa por MCP

En el MCP HTTP autenticado, llama a `import_session` con `agent` (`codex` u
`opencode`), `native_session_id` (el ID de una sesión existente) y `workspace`
(la ruta absoluta de un workspace de confianza). El Scheduler devuelve el
mismo `session_id` para el mismo agente, ID nativo y workspace efectivo, incluso
después de reiniciar.

Consulta el registro con `list_sessions` e `inspect_session`. Si el Scheduler ya
conoce la sesión, conserva su estado. Una referencia nueva queda en
`pending_confirmation` y bloquea los envíos y la reconexión. La importación
solo registra la referencia y prescinde de lanzar el agente o leer su
conversación.

## Encargo de seguimiento por MCP

Para un encargo adicional explícito del usuario, llama a `follow_up` con el
`session_id` devuelto por `delegate` y el nuevo `prompt`. La sesión debe
seguir conectada y haber terminado el turno anterior. `follow_up` devuelve
el mismo `session_id` y un nuevo `turn_id`.

`inspect_session` conserva el ID de sesión ACP y el historial de la
conversación. El estado y el resultado corresponden al turno más reciente.
En HTTP, cada `turn.completed` informa el `turn_id` de su propio turno.

## Cancelación y archivado por MCP

Llama a `cancel_turn` con el `session_id` y el `turn_id` del turno activo.
La sesión sigue visible en `list_sessions` con estado `cancelling` mientras
espera la respuesta final de ACP. El evento `turn.completed` usa el motivo
confirmado por el agente, incluido `end_turn` si el turno termina normalmente.

Cuando el turno alcance un desenlace confirmado, llama a `archive_session`
con el `session_id`. La sesión sale de `list_sessions` y sigue disponible
en `inspect_session` con `archived: true`, su historial y su desenlace.

El MCP HTTP conserva los metadatos del registro de sesiones en SQLite.
Tras reiniciar, la inspección conserva los identificadores y el último estado
conocido. El historial y los resultados observados durante la ejecución viven
en memoria. La cola de eventos pendientes se conserva por separado y continúa
sus reintentos con los IDs originales.

## Recuperación de un turno interrumpido

Si se pierde la respuesta del turno, el scheduler intenta cargar la sesión
ACP existente hasta tres veces, con hasta dos segundos para inicializar y cargar la sesión en cada intento.
Durante los intentos, `inspect_session` muestra `recovering` y conserva los
IDs y el historial ya observado. Si el desenlace sigue sin confirmarse, el
estado pasa a `unknown` y se registra `turn.recovery_required` con motivo
`outcome_unknown`. En HTTP, este evento tiene su propia suscripción y usa
la misma cola persistente que `turn.completed`.

Una carga válida conserva la conexión y amplía el historial observado con
las actualizaciones recuperadas que coinciden con el turno. Conserva el
orden del texto y de las actualizaciones de herramientas ya observadas.
Una carga puede omitir estados intermedios de una herramienta. El scheduler
usa el ID de la herramienta dentro del turno para incorporar los estados
recuperados compatibles. Si la carga no permite identificar el turno,
conserva el historial observado.

Si queda disponible la conexión y la sesión está sin un turno activo, una
llamada explícita a `follow_up` puede iniciar un turno con otro ID, aunque
el desenlace original siga indeterminado. En ese caso,
`inspect_session` conserva su ID en `unresolved_turn_ids` y muestra el
estado y resultado del nuevo turno en los campos habituales.

Llama a `reconnect_session` con `session_id` para reintentar el último
turno en `unknown`. Añade `turn_id` para recuperar un turno anterior
de `unresolved_turn_ids` cuando la sesión esté libre. `inspect_session`
muestra su estado, resultado y motivo de cierre en `previous_turns`,
por ID. Si el adaptador confirma su desenlace, `turn.completed` lleva
los IDs originales y el turno sale de `unresolved_turn_ids` al quedar
registrado el evento. Los campos del último turno conservan su valor.
Resuelve los turnos indeterminados antes de archivar la sesión. La
reconexión inicializa el adaptador y carga la sesión existente.

Tras reiniciar el servicio HTTP, el registro SQLite conserva los IDs,
el último estado conocido y el archivado. Usa `list_sessions` para
localizar sesiones activas e `inspect_session` para consultar sus
metadatos, incluso si están archivadas. Un turno sin desenlace confirmado
queda en `pending_confirmation` y bloquea `follow_up` y
`reconnect_session` para esa sesión. El historial y los resultados
permanecen en el agente.

ACP estándar puede recargar la conversación. Para confirmar el desenlace,
el scheduler requiere que la respuesta de `session/load` incluya
`_meta["session-scheduler/turnOutcome"]` con `sessionId`, `promptRequestId`
(el ID JSON-RPC original de `session/prompt`) y un `stopReason` terminal
reconocido. El fixture implementa esta evidencia. Un adaptador que solo
devuelve la recarga estándar deja el desenlace indeterminado.

## Registro local en Codex Desktop

En Windows, usa el arranque de desarrollo existente. El scheduler escucha por
Streamable HTTP en `http://127.0.0.1:<puerto>/mcp`, requiere un bearer y usa un
directorio de estado local con acceso restringido a su propietario.

En la configuración local de Codex, registra `[mcp_servers.session_scheduler]`
con esa URL, un `http_headers_helper` que obtenga el bearer del almacén de
credenciales y `default_tools_approval_mode = "prompt"`. En Codex Desktop
comprueba que aparecen las herramientas y ejecuta la consulta de solo lectura
`list_sessions`. Una lista vacía es un resultado válido.
[Configuración local y estado en Windows](docs/diseno-inicial.md#primera-validación-de-la-integración)

## Registro de la MCP App en ChatGPT y Dot

El registro local configura el cliente Codex Desktop. Para que Dot pueda
suscribirse a MCP Events, registra la MCP App en el workspace de ChatGPT
correspondiente y habilítala para el Dot de la cuenta conectada. Hay dos rutas
de entrada a `/mcp`.

- **Secure MCP Tunnel.** Asocia el túnel con el workspace de ChatGPT y la
  organización de Platform adecuados. Confirma permisos de uso y, si hace
  falta crear o administrar el túnel, de gestión. El cliente del túnel necesita
  su credencial de ejecución y acceso al servidor local. Conserva la
  autenticación del scheduler. [Secure MCP Tunnel](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels)
- **HTTPS alcanzable desde ChatGPT.** La URL debe llegar a `/mcp` con un
  certificado confiable para ChatGPT y autenticación compatible. La validación
  de esta ruta para Session Scheduler está en triage. La confianza de Windows
  en un certificado autofirmado solo cubre a los clientes que usan esa
  configuración local. [Issue #10](https://github.com/jerolan/session-scheduler/issues/10)

En ambas rutas registra evidencia por separado para el acceso de la app, el
challenge firmado que el scheduler envía al callback HTTPS y la entrega de
`turn.completed` tras un turno real autorizado. La reacción de Dot completa la
verificación de extremo a extremo. [MCP Events](https://developers.openai.com/plugins/build/mcp-events)

## Verificación

Usa [`verify-session-scheduler`](.agents/skills/verify-session-scheduler/SKILL.md)
para revisar el entorno sin cambios, ejecutar las pruebas existentes y, con
autorización, comprobar la suscripción y la entrega a Dot.

Para verificar OpenCode desde Windows, ejecuta la prueba con el binario de
desarrollo del scheduler y un perfil temporal de OpenCode con permisos
limitados al encargo de lectura. El receptor de loopback verifica el
challenge y la firma, y coteja los IDs de `turn.completed` con MCP.

```powershell
pwsh -NoProfile -File tests/windows_private_events_smoke.ps1 `
  -SchedulerExe $schedulerExe -OpenCodeExe $openCodeExe `
  -ProxyWorkspace $proxyWorkspace -ReadOnlyPrompt $prompt `
  -ExpectedOutput $expectedOutput
```

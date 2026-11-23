# Diseño inicial de Session Scheduler

Dot delega trabajo a Codex CLI y OpenCode mediante un plugin MCP. Session Scheduler registra las delegaciones, abre las sesiones por ACP y avisa a Dot cuando termina un turno.

## Workspaces y sesiones

- Cada sesión delegada pertenece a un workspace. El scheduler recibe el workspace en el que debe abrirla y usa su directorio de trabajo al crear la sesión ACP. El entorno que le proporciona el workspace se encarga de prepararlo.
- El flujo previsto acepta la delegación desde Windows y permite trabajar sobre un repositorio Linux por SSH cuando el workspace usa una carpeta proxy.
- Un workspace puede agrupar varias sesiones. La concurrencia entre ellas depende del workspace.

## Operaciones de Dot

- Delegar un encargo en un workspace y recibir los identificadores de sesión y turno sin esperar a que el agente termine.
- Listar e inspeccionar sesiones gestionadas y enviar nuevos encargos en una sesión existente.
- Cancelar un turno y esperar su desenlace antes de archivar su sesión. Archivar retira la sesión del listado activo y conserva el historial.
- Consultar el estado y pedir la reconexión de una sesión existente por MCP.
- Las solicitudes de permiso interactivo del agente se deniegan. El CLI de observación es opcional.

## Avisos y recuperación

- Dot puede suscribirse a los eventos de todas las delegaciones de su cuenta conectada. Cada turno con desenlace confirmado produce un evento con el identificador de sesión, el de turno y el motivo de cierre, incluido fallo o cancelación. Véase [un evento por turno](adr/0001-evento-por-turno.md).
- El scheduler conserva los eventos pendientes y reintenta su entrega con el mismo identificador tras un reinicio. Véase [entrega persistente](adr/0002-entrega-persistente-de-eventos.md).
- Si el scheduler pierde la respuesta de un turno, intenta recuperar la sesión y verificar el desenlace. Si agota la recuperación, avisa a Dot y ofrece la reconexión por MCP. Véase [recuperación de sesión](adr/0003-recuperacion-de-sesion.md).

## Primera validación de la integración

Conectar un plugin privado a Dot mediante Secure MCP Tunnel y verificar que Dot puede suscribirse, delegar un turno, recibir su evento y consultar la sesión. Repetir el recorrido con Codex CLI y OpenCode.

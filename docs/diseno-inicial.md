# Diseño inicial de Session Scheduler

La integración prevista permite a Dot delegar trabajo a Codex CLI y OpenCode mediante MCP. Session Scheduler registra las delegaciones y abre las sesiones por ACP. El descubrimiento y la entrega de avisos a Dot requieren validación independiente.

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

Session Scheduler y Codex Desktop se ejecutan de forma nativa en Windows. El scheduler ofrece Streamable HTTP en `127.0.0.1`, exige autenticación bearer y guarda su estado con acceso restringido a su propietario en una unidad local de Windows. La VM Linux aloja el repositorio de desarrollo y los workspaces a los que acceden las sesiones mediante SSH.

En Windows, `SESSION_SCHEDULER_STATE_DIR` debe señalar una ruta absoluta cuyo directorio padre ya exista y cuyos componentes no tengan puntos de reanálisis. El scheduler puede crear el directorio de estado con una DACL protegida para su cuenta de Windows. Si ya existe, comprueba la DACL antes de leer el estado.

La prueba local usa el ejecutable de Windows, un fixture ACP nativo y un receptor en loopback. Ejecutar `pwsh -File tests/windows_private_events_smoke.ps1 -SchedulerExe <exe> -FixtureExe <exe>` para verificar la firma y la correlación de los eventos, la persistencia de la suscripción tras un reinicio y las restricciones de acceso con una configuración desechable.

En la configuración local de Codex, declarar `[mcp_servers.session_scheduler]` con la URL de loopback, `http_headers_helper` para obtener el bearer del almacén de credenciales del usuario y `default_tools_approval_mode = "prompt"`. Comprobar el descubrimiento de herramientas en Desktop con una consulta de solo lectura. [Configuración MCP de Codex](https://learn.chatgpt.com/docs/extend/mcp)

La aceptación con el Dot conectado requiere comprobar que puede descubrir el servidor local, suscribirse y recibir el evento de un turno real. La demostración interactiva requiere una solicitud explícita del usuario. Las guías de Dot y MCP Events describen las computadoras conectadas, los plugins habilitados y la entrega por callback HTTPS. La compatibilidad de la suscripción con el servidor local de Codex queda por verificar. [Computadoras y aplicaciones de Dot](https://learn.chatgpt.com/docs/dots/computers-and-apps) · [MCP Events](https://developers.openai.com/plugins/build/mcp-events)

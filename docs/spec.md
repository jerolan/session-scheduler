# Session Scheduler para Dot

## Problema

Cuando Dot delega trabajo a un agente de programación, el turno puede terminar en segundo plano sin que Dot se entere. El usuario necesita encargar y seguir ese trabajo desde Dot, conservar la sesión para dar seguimiento y recibir un aviso cuando haya un desenlace o haga falta recuperar la sesión.

## Solución

Session Scheduler será una herramienta escrita en Rust que expone un plugin MCP privado para Dot y controla sesiones de Codex CLI y OpenCode mediante ACP. Dot puede delegar y administrar sesiones por workspace, recibir un MCP Event cuando termine cada turno y consultar o reconectar una sesión si se interrumpe. El usuario puede comprobar la integración desde su propio Dot con un encargo que devuelve `pong`.

## Historias de usuario

1. Como usuario, quiero conectar el MCP en Codex Desktop para poder usar la integración con mi Dot.
2. Como usuario, quiero pedirle a Dot que delegue un encargo en lenguaje natural para iniciar trabajo sin abrir manualmente el agente.
3. Como usuario, quiero poder delegar a Codex CLI para aprovecharlo en los proyectos donde lo uso.
4. Como usuario, quiero poder delegar a OpenCode para elegir el agente que corresponde al encargo.
5. Como Dot, quiero indicar el workspace apropiado para abrir la sesión en el entorno de trabajo del proyecto.
6. Como usuario, quiero poder delegar desde Windows con un workspace que usa una carpeta proxy para que el agente trabaje en el repositorio Linux por SSH.
7. Como usuario, quiero tener varias sesiones en un workspace para mantener separados encargos y conversaciones.
8. Como Dot, quiero recibir los identificadores de sesión y turno al iniciar la delegación para poder seguirla mientras el agente trabaja.
9. Como Dot, quiero listar las sesiones gestionadas y consultar su estado para encontrar la delegación de la que me habla el usuario.
10. Como Dot, quiero inspeccionar el resultado y el historial disponible para explicar qué hizo el agente.
11. Como Dot, quiero enviar un encargo adicional a una sesión existente para aprovechar su contexto.
12. Como usuario, quiero que el scheduler deniegue las solicitudes de permiso interactivo para que una delegación automática pueda avanzar con los permisos disponibles o reportar el bloqueo.
13. Como Dot, quiero una suscripción para todas las delegaciones de mi cuenta conectada para recibir avisos desde el primer turno.
14. Como Dot, quiero un evento por cada turno terminado para distinguir dos encargos ejecutados en la misma sesión.
15. Como Dot, quiero que el evento incluya los IDs de sesión y turno y el motivo de cierre para correlacionarlo con el encargo.
16. Como usuario, quiero que Dot se entere también de los fallos y cancelaciones para poder decidir cómo seguir.
17. Como Dot, quiero cancelar un turno activo para detener trabajo que el usuario ya no necesita.
18. Como usuario, quiero que una sesión siga visible mientras se cancela su turno para poder confirmar que el trabajo se detuvo antes de archivarla.
19. Como usuario, quiero archivar una sesión y conservar su historial para mantener despejado el listado activo.
20. Como usuario, quiero que un aviso pendiente sobreviva a una caída temporal o a un reinicio del scheduler para que Dot pueda recibirlo después.
21. Como Dot, quiero que un reintento conserve el ID del evento para reconocer entregas repetidas.
22. Como usuario, quiero que el scheduler intente recuperar automáticamente una sesión si pierde la respuesta del turno para reducir la intervención manual.
23. Como Dot, quiero recibir un aviso de recuperación requerida cuando se agoten los intentos para saber cuándo consultar la sesión.
24. Como Dot, quiero consultar el último estado conocido y pedir una reconexión por MCP para seguir trabajando con la sesión existente.
25. Como usuario, quiero que repetir un encargo requiera una solicitud explícita para evitar ejecutar dos veces un trabajo que pudo haber aplicado cambios.
26. Como usuario, quiero ver que mi Dot recibe el evento de una sesión real y me comunica el `pong` para comprobar que todo el recorrido funciona sin pedirle una actualización.

## Decisiones de implementación

- Session Scheduler y su servidor MCP se escribirán en Rust. Codex CLI y OpenCode son los dos agentes iniciales vía ACP.
- Cada sesión delegada pertenece a un workspace. El scheduler usa el workspace que le proporciona el entorno al abrir o reconectar la sesión. El entorno se encarga de prepararlo. La concurrencia entre sesiones depende del workspace.
- El flujo inicial admite delegar desde Windows y el uso de carpetas proxy cuyos repositorios canónicos se trabajan por SSH.
- Para la integración privada, Session Scheduler, su estado persistente, tunnel-client y Codex Desktop se ejecutan de forma nativa en Windows. La VM Linux aloja el repositorio de desarrollo y los workspaces a los que se accede por SSH.
- MCP es la interfaz principal. Expone delegación, listado, inspección, seguimiento, cancelación, archivado y reconexión de sesiones gestionadas. El registro conserva la relación entre workspace, sesión del scheduler, sesión ACP y turnos.
- El scheduler registra el turno antes de enviarlo y conserva su resultado cuando está disponible. Un encargo de seguimiento mantiene la sesión y recibe otro ID de turno.
- Las solicitudes de permiso interactivo se deniegan. El archivado de una sesión con trabajo activo requiere cancelar y esperar el desenlace del turno.
- El plugin privado se conecta a Codex Desktop y se habilita para el Dot del usuario. Secure MCP Tunnel es la vía elegida para la primera validación de la conexión privada.
- Dot se suscribe a los eventos de la cuenta conectada. El scheduler entrega MCP Events mediante el webhook firmado de la suscripción. Un turno con desenlace confirmado genera un evento de finalización con sus identificadores y su motivo de cierre.
- El evento pendiente se registra junto con el desenlace antes de intentar entregarlo. Los reintentos tras fallos temporales y reinicios reutilizan el ID del evento.
- Ante la pérdida de la respuesta, el scheduler intenta recuperar la misma sesión y confirmar el desenlace del turno. Reconectar conserva la conversación. Volver a ejecutar el encargo requiere una solicitud explícita.
- Si se agotan los intentos automáticos con el desenlace aún indeterminado, el scheduler emite un evento de recuperación requerida. Dot puede consultar el último estado y solicitar una reconexión. Una confirmación posterior produce el evento de finalización.

## Decisiones de pruebas

La interfaz MCP y la recepción del webhook son el punto principal de prueba. Los casos observan las respuestas de las herramientas, el estado consultable y los eventos que recibe el consumidor. El repositorio parte de un diseño nuevo y todavía carece de pruebas previas.

- **Aceptación con el Dot del usuario.** Con el MCP conectado en Codex Desktop y la suscripción activa en Dot, el usuario le pide a Dot que delegue una sesión y que el agente responda `pong`. El scheduler abre una sesión ACP real. Se comprueba que el webhook corresponde a los IDs de esa delegación y que Dot reacciona comunicando el `pong` sin una segunda solicitud del usuario.
- **Compatibilidad inicial.** Ejercitar la creación de sesión, el turno y la entrega del evento con Codex CLI y OpenCode. Verificar que un segundo turno conserva el ID de sesión y se reconoce por su propio ID. Incluir un workspace de Windows con carpeta proxy a Linux.
- **Administración.** Verificar vía MCP el listado, la consulta, el permiso interactivo denegado, la cancelación y el archivado. Una sesión con trabajo activo sigue visible hasta que el turno alcanza un desenlace y se archiva. El historial sigue disponible.
- **Entrega persistente.** Simular un receptor temporalmente inaccesible y reiniciar el scheduler. Comprobar que el aviso llega al recuperarse el receptor, con el mismo ID.
- **Recuperación.** Interrumpir la conexión ACP antes de la respuesta final. Comprobar la recuperación automática cuando hay evidencia del desenlace y el evento de recuperación requerida cuando se agotan los intentos. Verificar por MCP que la reconexión conserva la sesión y que recuperar la sesión evita repetir el encargo.

## Fuera de alcance

- Hacer del CLI de observación un requisito de la primera versión.
- Aprovisionar workspaces, administrar sus checkouts o imponer una sola política de concurrencia.
- Gestionar sesiones que se abrieron fuera de Session Scheduler o borrar definitivamente su historial.
- Definir la reacción de Dot más allá de verificar que pueda actuar al recibir el evento.
- Dar soporte inicial a agentes distintos de Codex CLI y OpenCode o publicar el plugin en el directorio público.

## Notas

El resultado de un turno y la entrega de su evento se observan por separado. Recuperar el historial ACP puede restablecer la sesión aunque el desenlace del turno siga indeterminado. El ensayo con el Dot del usuario comprueba la suscripción, la recepción y la reacción completa.

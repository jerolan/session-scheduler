# Recuperación automática y reconexión por MCP

Si se pierde la respuesta de un turno, Session Scheduler intenta recuperar la misma sesión y confirmar su desenlace sin reenviar el encargo. Al agotar los intentos, emite un MCP Event de recuperación requerida y permite a Dot consultar el estado y pedir una reconexión por MCP. El evento de finalización se emite cuando se confirma el desenlace del turno.

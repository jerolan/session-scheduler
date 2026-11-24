# Conservar los eventos pendientes de entrega

Dot puede estar temporalmente inaccesible cuando termina un turno. Session Scheduler registra el evento pendiente junto con el desenlace del turno antes del primer intento y conserva lo necesario para reintentar después de un reinicio. Cada reintento reutiliza el mismo ID para que el receptor reconozca entregas repetidas.

Si el almacenamiento falla para un turno con suscripciones vigentes, el
scheduler registra el error y muestra `completion_pending`. La sesión queda
indisponible para seguimiento mientras el worker reintenta guardar el mismo
evento. Al confirmar la escritura, el scheduler publica el desenlace y habilita
la entrega. Si el proceso se detiene antes de guardar el evento, la recuperación
al reiniciar solo puede leer los eventos que quedaron en disco. Los turnos sin
suscripciones vigentes se finalizan sin una escritura en la cola.

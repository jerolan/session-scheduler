# Conservar los eventos pendientes de entrega

Dot puede estar temporalmente inaccesible cuando termina un turno. Session Scheduler registra el evento pendiente junto con el desenlace del turno antes del primer intento y conserva lo necesario para reintentar después de un reinicio. Cada reintento reutiliza el mismo ID para que el receptor reconozca entregas repetidas.

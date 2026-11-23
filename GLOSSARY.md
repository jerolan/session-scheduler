# Session Scheduler

Session Scheduler registra el trabajo que Dot delega a agentes de programación y le informa cuando ese trabajo termina.

## Lenguaje

**Sesión delegada**  
Sesión de un agente de programación a la que Dot encarga trabajo a través de Session Scheduler.

**Turno delegado**

Trabajo que un agente realiza para responder a un encargo de Dot dentro de una sesión delegada.

**Evento de finalización**  
Aviso que Session Scheduler emite hacia Dot cuando termina un turno delegado, incluso si el turno falla o se cancela.

**Suscripción de finalización**

Suscripción de Dot a los eventos de finalización de las delegaciones de su cuenta conectada.

**Sesión archivada**

Sesión delegada que deja de aparecer en el listado activo y conserva su historial.

**Workspace**

Entorno de trabajo de un proyecto en el que se abren y se agrupan sus sesiones delegadas.

**Reconexión de sesión**

Restablecimiento del acceso a una sesión delegada existente para inspeccionar su historial o enviarle otro encargo.

**Recuperación requerida**

Estado de un turno cuyo desenlace sigue indeterminado después de agotar los intentos automáticos de recuperación.

# Pruebas manuales del PR #14

Fecha: 18 de octubre de 2026. Plataforma: Windows, ventana nativa con WebView2.

Se compiló el código del PR y se lanzó una copia con APPDATA y LOCALAPPDATA
temporales, portapapeles desactivado y tres torrents ficticios inicialmente
pausados. Para convivir con las instancias del usuario, el binario de prueba
usó el puerto de instancia única 47832 en lugar de 47831; el archivo fuente
se restauró después de compilar. No se modificaron las descargas del usuario.

## Resultados

- Seleccionar las casillas de dos filas: ambas quedan marcadas y el contador
  muestra «2 seleccionados». Los botones muestran «Reanudar (2)», «Pausar (2)»
  y «Quitar (2)».
- Reanudar los seleccionados: los dos pasan a obtener metadatos y el tercero
  sigue pausado. Se comprobó en la ventana y en la API local.
- Pausar los seleccionados: ambos vuelven al estado pausado.
- Quitar los seleccionados: el diálogo identifica los dos nombres y ofrece
  eliminar los archivos con la casilla desmarcada. Cancelar conserva los tres.
- Seleccionar visibles: quedan seleccionados los tres torrents.
- Cerrar con la X tras reanudar: la ventana deja de aparecer en la lista de
  ventanas, el mismo proceso sigue respondiendo y la API mantiene los tres
  torrents en estado de obtención de metadatos. El estado guardado contiene
  los tres con `paused: false`. No hubo errores de creación de bandeja.
- Abrir otra vez el ejecutable: recupera la misma ventana (mismo identificador)
  y conserva un único proceso de prueba.
- Eliminación por API local con `delete=false`: se quitaron las dos entradas
  ficticias y quedó solo la tercera en la ventana; un archivo de control en
  la carpeta temporal se conservó. La confirmación final de eliminación por
  botón no se ejecutó en esta prueba.

## Límites

El control de Windows disponible no expone el área de notificación como una
ventana controlable. No se verificaron el clic en el icono, el menú «Mostrar
Huillín» ni «Salir» desde la bandeja. La recuperación comprobada fue por
instancia única. Tampoco se simuló un fallo de creación del icono ni se midió
transferencia de datos en segundo plano: los torrents eran ficticios, sin peers.

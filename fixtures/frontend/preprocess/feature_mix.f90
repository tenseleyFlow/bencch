#define GREETING 'bench says hi'
#define EXTRA_LINE 1
#include "include_message.inc"

program macro_include
  implicit none
#ifdef EXTRA_LINE
  print *, 'extra branch'
#endif
  print *, GREETING
  print *, INCLUDED_MSG
end program macro_include

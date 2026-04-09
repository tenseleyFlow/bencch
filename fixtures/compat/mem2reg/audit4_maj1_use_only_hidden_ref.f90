! Audit #4 MAJOR-1 — USE ONLY filtered names now produce a
! compile-time error.
!
! Fixed: lower.rs computes a `filtered_names` set per scope by
! diffing each `use mod, only: ...` against that module's
! exported globals. A pre-pass walks the function body before
! lowering and emits a hard compile-time error mentioning the
! filtered name when any reference to it is detected. Per
! F2018 §11.2.2, USE association brings in only the listed
! names — anything else must be diagnosed.
!
! ERROR_EXPECTED: hidden
program audit4_maj1_use_only_hidden_ref
  use audit4_maj1_mod, only: shown
  implicit none
  print *, shown
  print *, hidden
end program

module audit4_maj1_mod
  integer :: shown = 7
  integer :: hidden = 99
end module audit4_maj1_mod

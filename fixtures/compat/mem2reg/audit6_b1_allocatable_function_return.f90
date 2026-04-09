! Audit #6 BLOCKING-1 — functions returning allocatable arrays
! crash IR verification before compilation completes.
!
! Root cause (suspected): in lower_unit's Function arm, the
! result variable is alloca'd with the array element type
! (e.g., i32) instead of the allocatable descriptor type
! (`Ptr<[i8 x 384]>`). When the function body assigns into the
! result via `r = [1,2,3,4,5]`, the store's value type
! (Ptr<[i32 x 5]>) doesn't match the alloca's pointee (i32),
! and the IR verifier rightfully rejects the program with:
!
!   IR verify: store %3: value type ptr<i32> doesn't match pointee type i32
!
! Expected: compiles, runs, prints "1 2 3 4 5".
!
! XFAIL: audit6 BLOCKING-1 (allocatable function return type)
! CHECK: 1 2 3 4 5
program audit6_b1_allocatable_function_return
  implicit none
  integer, allocatable :: arr(:)
  arr = get_array()
  print *, arr
contains
  function get_array() result(r)
    integer, allocatable :: r(:)
    allocate(r(5))
    r = [1, 2, 3, 4, 5]
  end function
end program

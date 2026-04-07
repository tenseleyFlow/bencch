! Structure constructor test: construct derived type with positional values.
!
! CHECK: 99
! CHECK: 2.71
program test_struct_constructor
    implicit none

    type :: pair
        integer :: key
        real :: val
    end type

    type(pair) :: p
    p = pair(99, 2.71)
    print *, p%key
    print *, p%val
end program

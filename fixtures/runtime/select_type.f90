! SELECT TYPE construct with TYPE IS and CLASS IS guards.
! Tests: exact type matching, parent type matching via inheritance.
!
! CHECK: circle
! CHECK: 5.0
! CHECK: is a shape
program test_select_type
    implicit none
    type :: shape
        real :: x, y
    end type
    type, extends(shape) :: circle
        real :: radius
    end type

    type(circle) :: c
    c%x = 1.0
    c%y = 2.0
    c%radius = 5.0

    ! TYPE IS: exact type match.
    select type (c)
    type is (circle)
        print *, 'circle'
        print *, c%radius
    type is (shape)
        print *, 'shape'
    end select

    ! CLASS IS: matches type or any extension.
    select type (c)
    class is (shape)
        print *, 'is a shape'
    class default
        print *, 'unknown'
    end select
end program

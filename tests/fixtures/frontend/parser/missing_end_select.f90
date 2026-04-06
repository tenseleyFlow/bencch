program broken_select
  implicit none
  integer :: x
  x = 1

  select case (x)
  case (1)
    print *, 'one'
end program broken_select
